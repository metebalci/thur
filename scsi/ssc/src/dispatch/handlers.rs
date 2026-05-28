// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-opcode SCSI handlers shared by both tape products. Each
//! handler is dispatched from the product's match-tree in protocol.rs
//! based on `ctx.cdb[0]`. They run on the tokio blocking thread pool
//! — `serve_connection` wraps `handle_scsi_command` in
//! `tokio::task::spawn_blocking` so the sync `Cartridge` operations
//! (which can park on `PoolBudget` for up to
//! `backpressure_max_wait_seconds`) never wedge an async worker. No
//! `await` allowed inside any handler.
//!
//! These handlers have **no library / element_config /
//! diagnostic_store access** — they touch
//! [`DriveManager`](crate::drive_manager::DriveManager) per-drive
//! state, the [`TapeDeviceFacade`](core_mediachanger::TapeDeviceFacade) on
//! `ctx.facade`, and SCSI helpers only. thurvtl invokes them
//! on a plain [`ScsiCtx`]. Handlers that still need
//! the SMC-side `Library` lock (drive MODE SENSE / MODE SELECT pages,
//! INQUIRY VPD `0xB4`) or the per-LUN `DiagnosticStore`
//! (SEND/RECEIVE DIAGNOSTIC, LOG SELECT, changer-LUN LOG SENSE) stay
//! in `vtl/daemon/src/iscsi/protocol.rs` along with the
//! SMC changer ops (INITIALIZE/READ ELEMENT STATUS, MOVE/EXCHANGE
//! MEDIUM, SEND VOLUME TAG).

use anyhow::{Result, anyhow};

use crate::drive_manager;
use crate::scsi;

use super::audit::audit_append;
use super::types::{
    ScsiCtx, ScsiResp, ScsiStatus, drive_mfg_serial_fallback, limit_len, pdu_expected_xfer_len,
    put_be_u32,
};

use core_mediachanger::{AuditResult, TapeEvent};

pub fn handle_test_unit_ready(_ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    Ok(ScsiResp::good())
}

pub fn handle_request_sense(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;

    // REQUEST SENSE
    let alloc = cdb[4] as u32;
    tracing::debug!("REQUEST SENSE: LUN={}, allocation_length={}", lun, alloc);

    // For MVP, return "no sense" if no pending sense data
    // Real implementation would track pending sense per session/LUN
    let sense_data = scsi::sense::build_sense(
        scsi::sense::SenseKey::NoSense,
        scsi::sense::ASC_NO_ADDITIONAL_INFO,
    );

    Ok(ScsiResp {
        status: ScsiStatus::Good,
        data_out: limit_len(sense_data, alloc),
        sense: None,
    })
}

pub fn handle_read_block_limits(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    // READ BLOCK LIMITS (SSC) — Linux st driver issues this on every
    // attach to learn the device's block-size constraints. Reply with
    // a 6-byte response: granularity (1 byte), max block length (3
    // bytes), min block length (2 bytes).
    //
    // We're a variable-block target, so:
    //   granularity = 0 (no power-of-2 step)
    //   max         = 16 MiB - 1 (fits in 24 bits, well above any
    //                 realistic tape block; also exceeds our default
    //                 chunk size so the kernel never artificially
    //                 splits writes)
    //   min         = 1
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::check_condition());
    }
    let max_block: u32 = 0x00FF_FFFF; // 16 MiB - 1
    let min_block: u16 = 1;
    let mut d = vec![0u8; 6];
    d[0] = 0; // granularity
    d[1] = ((max_block >> 16) & 0xFF) as u8;
    d[2] = ((max_block >> 8) & 0xFF) as u8;
    d[3] = (max_block & 0xFF) as u8;
    d[4..6].copy_from_slice(&min_block.to_be_bytes());
    Ok(ScsiResp {
        status: ScsiStatus::Good,
        data_out: d,
        sense: None,
    })
}

pub fn handle_report_density_support(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;

    // REPORT DENSITY SUPPORT (SSC §7.13) — backup software queries this
    // to learn which LTO densities the drive can read/write. We report
    // a single descriptor that matches the cartridge's actual LTO
    // generation (set at cartridge creation) plus the previous
    // generation (LTO-N also reads LTO-(N-1) per the LTO standard).
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::check_condition());
    }
    let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as u32;
    let media_only = (cdb[1] & 0x01) != 0; // MEDIA bit: only media-loaded densities
    let _media_type = (cdb[1] & 0x02) != 0; // MEDIUM_TYPE bit (we ignore for now)

    // Build a descriptor for LTO generation N (52 bytes per SSC-4
    // §7.13.2). We can issue this whether a cartridge is loaded
    // or not — when MEDIA=1 we'd normally restrict to the loaded
    // tape's densities, but since our INQUIRY always reports
    // LTO-8 we always include the LTO-8 + LTO-7 pair.
    let _ = media_only;

    // LTO density codes (from LTO Consortium spec):
    //   LTO-5: 0x58, LTO-6: 0x5A, LTO-7: 0x5C, LTO-7 Type M: 0x5D,
    //   LTO-8: 0x5E.
    // Our daemon emulates LTO-8 by default; emit the LTO-8 (RW)
    // and LTO-7 (RO) pair which matches real LTO-8 drives.
    let descriptors: &[(u8, &str, &str, u8)] = &[
        (0x5E, shared_naming::VENDOR_INQUIRY, "Ultrium 8       ", 1), // primary, RW
        (0x5C, shared_naming::VENDOR_INQUIRY, "Ultrium 7       ", 0), // secondary, RO (DEFLT=0)
    ];

    let header_len = 4u32;
    let desc_len = 52u32;
    let total_payload_len = (descriptors.len() as u32) * desc_len;
    let mut d = Vec::with_capacity((header_len + total_payload_len) as usize);

    // Header: Available Length = total_payload_len + 2 reserved bytes
    let avail = total_payload_len + 2;
    d.extend_from_slice(&(avail as u16).to_be_bytes());
    d.push(0x00); // reserved
    d.push(0x00); // reserved

    for (i, (code, vendor, product, default)) in descriptors.iter().enumerate() {
        let mut desc = vec![0u8; 52];
        desc[0] = *code; // Primary density code
        desc[1] = if i == 0 { *code } else { 0x00 }; // Secondary density code
        // Bit layout byte 2: WRTOK | DUP | DEFLT
        let wrtok = if i == 0 { 0x80 } else { 0x00 };
        let deflt = if *default != 0 { 0x20 } else { 0x00 };
        desc[2] = wrtok | deflt;
        // bytes 3-4 reserved
        // bytes 5-7: bits per mm (0)
        // bytes 8-9: media width in mm (12.65 mm * 10 = ~127)
        desc[8..10].copy_from_slice(&127u16.to_be_bytes());
        // bytes 10-11: tracks (LTO-8 = 6656 wraps; report 0 = unknown)
        // bytes 12-15: capacity in MB (LTO-8 native = 12 TB ≈ 12000000)
        let capacity_mb: u32 = match *code {
            0x5E => 12_000_000, // LTO-8
            0x5C => 6_000_000,  // LTO-7
            _ => 0,
        };
        desc[12..16].copy_from_slice(&capacity_mb.to_be_bytes());
        // bytes 16-23: vendor identification (8 bytes, ASCII, space-padded)
        let vbytes = vendor.as_bytes();
        desc[16..16 + vbytes.len().min(8)].copy_from_slice(&vbytes[..vbytes.len().min(8)]);
        for slot in &mut desc[16 + vbytes.len().min(8)..24] {
            *slot = b' ';
        }
        // bytes 24-31: product identification (8 bytes)
        let pbytes = product.as_bytes();
        desc[24..24 + pbytes.len().min(8)].copy_from_slice(&pbytes[..pbytes.len().min(8)]);
        for slot in &mut desc[24 + pbytes.len().min(8)..32] {
            *slot = b' ';
        }
        // bytes 32-51: description (20 bytes, can be empty)
        d.extend_from_slice(&desc);
    }

    Ok(ScsiResp {
        status: ScsiStatus::Good,
        data_out: limit_len(d, alloc),
        sense: None,
    })
}

pub fn handle_rewind(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;
    let event_tx = ctx.event_tx;

    // REWIND
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::good());
    }
    match drive_manager.with_drive(drive_id, tsih, |cart| {
        let old_lba = cart.head_lba();
        cart.rewind();
        Ok((cart.label().to_string(), old_lba, cart.head_lba()))
    }) {
        Ok((tape_id, old_lba, new_lba)) => {
            // Emit HeadPositionChanged event
            let _ = event_tx.send(TapeEvent::HeadPositionChanged {
                tape_id,
                old_lba,
                new_lba,
                reason: core_mediachanger::PositionChangeReason::Rewind,
            });
            Ok(ScsiResp::good())
        }
        Err(e) => Err(anyhow!("REWIND failed: {}", e)),
    }
}

pub fn handle_load_unload(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;
    let event_tx = ctx.event_tx;

    // LOAD/UNLOAD (SSC-4 §7.3). cdb[4] bit 0 (LOAD) selects:
    //   1 = LOAD (rewind, position to BOT, leave loaded)
    //   0 = UNLOAD (rewind and eject — `mt offline`)
    // bits 1 (EOT), 2 (RE-TENSION), 3 (HOLD) are advisory. We
    // ignore HOLD, treat RE-TENSION as a rewind, and otherwise
    // do the canonical action. UNLOAD must call into
    // drive_manager so the cartridge's `Drop` runs (chunk
    // sealing, manifest persist, dirty-page flush) and emit
    // CartridgeUnloaded so the upload pipeline can finalize.
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::good());
    }
    let load = (cdb[4] & 0x01) != 0;
    if load {
        match drive_manager.with_drive(drive_id, tsih, |cart| {
            cart.rewind();
            Ok(())
        }) {
            Ok(()) => Ok(ScsiResp::good()),
            Err(e) => {
                tracing::warn!("LOAD (0x1B, LOAD=1) failed: {}", e);
                Ok(ScsiResp::check_condition_for(&e))
            }
        }
    } else {
        // PREVENT/ALLOW: refuse UNLOAD if any active session has
        // asserted bit 0 (data-transport removal prevent) on this
        // drive. SPC-4 §6.13 / SSC-5 §7.3 — host sees ILLEGAL
        // REQUEST + ASC/ASCQ 0x53/0x02 (MEDIUM REMOVAL PREVENTED).
        if drive_manager.is_data_transport_prevented(drive_id) {
            tracing::warn!(
                "UNLOAD (0x1B, LOAD=0) refused on drive {}: data-transport removal prevented",
                drive_id
            );
            let sense = scsi::sense::SenseDataBuilder::new(
                scsi::sense::SenseKey::IllegalRequest,
                scsi::sense::ASC_MEDIUM_REMOVAL_PREVENTED,
            )
            .build();
            return Ok(ScsiResp::check_condition_with_sense(sense));
        }
        // Capture barcode while cart is still loaded so
        // the CartridgeUnloaded event carries a real
        // tape_id. Then call unload_cartridge which
        // drops the Cartridge (sealing/flushing).
        let label_before = drive_manager.get_cartridge_label(drive_id).ok().flatten();
        match drive_manager.unload_cartridge(drive_id) {
            Ok(label) => {
                let tape_id = label_before.unwrap_or(label);
                let _ = event_tx.send(TapeEvent::CartridgeUnloaded {
                    tape_id,
                    drive_num: drive_id as u8,
                });
                Ok(ScsiResp::good())
            }
            Err(e) => {
                tracing::warn!("UNLOAD (0x1B, LOAD=0) failed: {}", e);
                Ok(ScsiResp::check_condition_for(&e))
            }
        }
    }
}

pub fn handle_read_position(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let lun = ctx.lun;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;
    let cdb = ctx.cdb;

    // READ POSITION (SSC-5 §7.10). Service Action in CDB byte 1
    // lower 5 bits:
    //   0x00 / 0x01  Short Form (20 bytes, 32-bit LBA)
    //   0x06         Long Form  (32 bytes, 64-bit partition / block /
    //                file / set numbers)
    //   0x08         Extended Form (32 bytes, 64-bit first/last LBA
    //                + 64-bit buffer-bytes count)
    //
    // Long form is required when block counts cross 32-bit (e.g.
    // very small block sizes at LTO-8 capacities); backup software
    // (LTFS, NetBackup) falls back to it when the short form
    // returns BPU.
    let svc_action = cdb[1] & 0x1F;
    let alloc = pdu_expected_xfer_len(ctx.pdu);

    let d = match svc_action {
        0x00 | 0x01 => {
            // Short Form (20 bytes). Layout per SSC-5 §7.10.2:
            //   byte 0: BOP/EOP/BPU/PERR/BYCU/LOCU/.../BPEW flags
            //   byte 1: active partition number
            //   bytes 4..8: First block location (u32 BE)
            //   bytes 8..12: Last block location (= same here)
            // Position is a u64 internally — when it exceeds u32::MAX
            // the short form can't represent it; set BPU and zero the
            // position fields rather than truncate to a wrong value.
            let mut d = vec![0u8; 20];
            if lun >= 1 {
                drive_manager
                    .with_drive(drive_id, tsih, |cart| {
                        let pos = cart.position();
                        if pos == 0 {
                            d[0] |= 0x80; // BOP
                        }
                        if cart.at_eod() {
                            d[0] |= 0x40; // EOP
                        }
                        d[1] = cart.active_partition();
                        if pos > u32::MAX as u64 {
                            d[0] |= 0x04; // BPU — Block Position Unknown
                        } else {
                            put_be_u32(&mut d[4..8], pos as u32);
                            put_be_u32(&mut d[8..12], pos as u32);
                        }
                        Ok(())
                    })
                    .map_err(|e| anyhow!("READ POSITION failed: {}", e))?;
            }
            d
        }
        0x06 => {
            // Long Form (32 bytes, SSC-5 §7.10.4):
            //   byte 0:   BOP (b7) | EOP (b6) | MPU (b3) | BPU (b2)
            //   bytes 4..8:   Partition number (u32 BE)
            //   bytes 8..16:  Block number in partition (u64 BE)
            //   bytes 16..24: File number in partition (u64 BE)
            //   bytes 24..32: Set number in partition (u64 BE)
            // We don't track file/set numbers — set MPU so the host
            // doesn't trust them.
            let mut d = vec![0u8; 32];
            if lun >= 1 {
                drive_manager
                    .with_drive(drive_id, tsih, |cart| {
                        let pos = cart.position();
                        if pos == 0 {
                            d[0] |= 0x80; // BOP
                        }
                        if cart.at_eod() {
                            d[0] |= 0x40; // EOP
                        }
                        // MPU — file/set numbers unknown.
                        d[0] |= 0x08;
                        put_be_u32(&mut d[4..8], cart.active_partition() as u32);
                        d[8..16].copy_from_slice(&pos.to_be_bytes());
                        // file/set numbers stay zero (MPU set).
                        Ok(())
                    })
                    .map_err(|e| anyhow!("READ POSITION failed: {}", e))?;
            }
            d
        }
        0x08 => {
            // Extended Form (32 bytes, SSC-5 §7.10.3):
            //   byte 0:   BOP (b7) | EOP (b6) | LOCU (b5) | BYCU (b4) | RBPU (b0)
            //   byte 1:   Partition number
            //   bytes 2..4:  Additional length (= 28)
            //   bytes 5..8:  Number of buffer blocks (24-bit BE)
            //   bytes 8..16:  First block location (u64 BE)
            //   bytes 16..24: Last block location (u64 BE)
            //   bytes 24..32: Number of buffer bytes (u64 BE)
            // Buffer counts (LOCU/BYCU) — virtual drive holds nothing
            // in a buffer at this granularity, so set LOCU+BYCU to 1
            // ("location/bytes count not valid") and leave the buffer
            // fields zero. Some real LTO drives report stale buffer
            // depth here under burst writes.
            let mut d = vec![0u8; 32];
            d[2] = 0;
            d[3] = 28; // additional length
            if lun >= 1 {
                drive_manager
                    .with_drive(drive_id, tsih, |cart| {
                        let pos = cart.position();
                        if pos == 0 {
                            d[0] |= 0x80; // BOP
                        }
                        if cart.at_eod() {
                            d[0] |= 0x40; // EOP
                        }
                        d[0] |= 0x20; // LOCU — buffer block count not valid
                        d[0] |= 0x10; // BYCU — buffer byte count not valid
                        d[1] = cart.active_partition();
                        d[8..16].copy_from_slice(&pos.to_be_bytes());
                        d[16..24].copy_from_slice(&pos.to_be_bytes());
                        Ok(())
                    })
                    .map_err(|e| anyhow!("READ POSITION failed: {}", e))?;
            }
            d
        }
        _ => {
            // Unsupported service action — INVALID FIELD IN CDB.
            return Ok(ScsiResp::check_condition_for(
                &core_mediachanger::errors::SmcError::InvalidField,
            ));
        }
    };

    Ok(ScsiResp {
        status: ScsiStatus::Good,
        data_out: limit_len(d, alloc),
        sense: None,
    })
}

pub fn handle_space_6(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;
    let event_tx = ctx.event_tx;

    // SPACE (6)
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::good());
    }
    let code = cdb[1] & 0x0F;
    // SPACE(6) count is signed 24-bit (SSC §7.5). Sign-extend so
    // `mt bsf 1` (count = -1) walks one filemark backward instead
    // of forward 16M filemarks.
    let raw = ((cdb[2] as u32) << 16) | ((cdb[3] as u32) << 8) | (cdb[4] as u32);
    let count = if raw & 0x0080_0000 != 0 {
        (raw | 0xFF00_0000) as i32
    } else {
        raw as i32
    };
    match drive_manager.with_drive(drive_id, tsih, |cart| {
        let old_lba = cart.head_lba();
        let moved = match code {
            0x00 => cart.space_records(count as i64),
            0x01 => cart.space_filemarks(count as i64),
            0x03 => {
                cart.space_to_eod();
                count as i64
            }
            _ => 0,
        };
        Ok((cart.label().to_string(), old_lba, cart.head_lba(), moved))
    }) {
        Ok((tape_id, old_lba, new_lba, moved)) => {
            let _ = event_tx.send(TapeEvent::HeadPositionChanged {
                tape_id,
                old_lba,
                new_lba,
                reason: core_mediachanger::PositionChangeReason::Space,
            });
            // SPACE residual (SSC-5 §7.5). If fewer demarcations were
            // traversed than requested, terminate with CHECK CONDITION
            // and report (count − moved) in INFORMATION so the host's
            // tape-position tracking (Linux st: `drv_file += count` on
            // success, `drv_file -= residual` on CC) can compute the
            // real position. Without this, e.g. Linux's slow MTEOM path
            // emits SPACE FILEMARKS count=0x7FFFFF and our success
            // response makes the kernel believe 8388607 filemarks were
            // crossed — surfaces in bareos as the spurious
            // "files mismatch! Volume=8388607" diagnostic (issue #33).
            // EOD code (0x03) has no residual concept.
            if (code == 0x00 || code == 0x01) && (moved as i32) != count {
                let residual = count.wrapping_sub(moved as i32) as u32;
                let sense = scsi::sense::SenseDataBuilder::new(
                    scsi::sense::SenseKey::BlankCheck,
                    scsi::sense::ASC_EOD_DETECTED,
                )
                .with_information(residual)
                .build();
                return Ok(ScsiResp::check_condition_with_sense(sense));
            }
            Ok(ScsiResp::good())
        }
        Err(e) => {
            tracing::warn!("SPACE(6) failed: {}", e);
            Ok(ScsiResp::check_condition_for(&e))
        }
    }
}

pub fn handle_space_16(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;
    let event_tx = ctx.event_tx;

    // SPACE (16) — same as SPACE(6) with an 8-byte signed count in cdb[4..12].
    // LTO-7+ frequently uses this form because the 24-bit count of the
    // 6-byte form is too small for large tapes.
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::good());
    }
    let code = cdb[1] & 0x0F;
    let count = i64::from_be_bytes([
        cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9], cdb[10], cdb[11],
    ]);
    match drive_manager.with_drive(drive_id, tsih, |cart| {
        let old_lba = cart.head_lba();
        let moved = match code {
            0x00 => cart.space_records(count),
            0x01 => cart.space_filemarks(count),
            0x03 => {
                cart.space_to_eod();
                count
            }
            _ => 0,
        };
        Ok((cart.label().to_string(), old_lba, cart.head_lba(), moved))
    }) {
        Ok((tape_id, old_lba, new_lba, moved)) => {
            let _ = event_tx.send(TapeEvent::HeadPositionChanged {
                tape_id,
                old_lba,
                new_lba,
                reason: core_mediachanger::PositionChangeReason::Space,
            });
            // Same residual semantics as SPACE(6) — see commentary
            // there for the bareos / Linux-st context.
            if (code == 0x00 || code == 0x01) && moved != count {
                let residual = (count.wrapping_sub(moved) & 0xFFFF_FFFF) as u32;
                let sense = scsi::sense::SenseDataBuilder::new(
                    scsi::sense::SenseKey::BlankCheck,
                    scsi::sense::ASC_EOD_DETECTED,
                )
                .with_information(residual)
                .build();
                return Ok(ScsiResp::check_condition_with_sense(sense));
            }
            Ok(ScsiResp::good())
        }
        Err(e) => {
            tracing::warn!("SPACE(16) failed: {}", e);
            Ok(ScsiResp::check_condition_for(&e))
        }
    }
}

pub fn handle_write_filemarks_6(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;

    // WRITE FILEMARKS (6)
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::good());
    }
    if let Err(e) = drive_manager.enforce_write_mode(drive_id, tsih) {
        tracing::warn!("WRITE FILEMARKS(6) refused by drive write-mode: {}", e);
        return Ok(ScsiResp::check_condition_for(&e));
    }
    let count = ((cdb[2] as u32) << 16) | ((cdb[3] as u32) << 8) | (cdb[4] as u32);
    match drive_manager.with_drive(drive_id, tsih, |cart| {
        for _ in 0..count {
            cart.write_filemark()?;
        }
        Ok(())
    }) {
        Ok(()) => Ok(ScsiResp::good()),
        Err(e) => {
            tracing::warn!("WRITE FILEMARKS(6) failed: {}", e);
            Ok(ScsiResp::check_condition_for(&e))
        }
    }
}

pub fn handle_write_filemarks_16(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;

    // WRITE FILEMARKS (16) — per SSC-4 §7.4 (Table 75) the
    // TRANSFER LENGTH (filemark count) is a 4-byte unsigned
    // big-endian field at cdb[12..16]. cdb[2..12] are reserved.
    // The earlier "5-byte at cdb[6..11]" reading was wrong —
    // it pulled count from the reserved region, so any host
    // using the 16-byte form (LTO-7+ for large counts) wrote
    // garbage filemark counts.
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::good());
    }
    if let Err(e) = drive_manager.enforce_write_mode(drive_id, tsih) {
        tracing::warn!("WRITE FILEMARKS(16) refused by drive write-mode: {}", e);
        return Ok(ScsiResp::check_condition_for(&e));
    }
    let count = u32::from_be_bytes([cdb[12], cdb[13], cdb[14], cdb[15]]) as u64;
    match drive_manager.with_drive(drive_id, tsih, |cart| {
        for _ in 0..count {
            cart.write_filemark()?;
        }
        Ok(())
    }) {
        Ok(()) => Ok(ScsiResp::good()),
        Err(e) => {
            tracing::warn!("WRITE FILEMARKS(16) failed: {}", e);
            Ok(ScsiResp::check_condition_for(&e))
        }
    }
}

pub fn handle_locate_10(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;
    let event_tx = ctx.event_tx;

    // LOCATE(10) — SSC §7.7. cdb[1] bit 1 (CP) selects "change
    // partition"; if set, cdb[8] holds the destination partition
    // number. LTFS uses CP to switch between P0 (index) and P1 (data).
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::good());
    }
    let cp = (cdb[1] & 0x02) != 0;
    let target_lba = u32::from_be_bytes([cdb[3], cdb[4], cdb[5], cdb[6]]) as u64;
    let partition = cdb[8];
    match drive_manager.with_drive(drive_id, tsih, |cart| {
        let old_lba = cart.head_lba();
        let result = if cp {
            cart.locate_partition(partition, target_lba)
        } else {
            cart.locate(target_lba)
        };
        result.map(|()| (cart.label().to_string(), old_lba, cart.head_lba()))
    }) {
        Ok((tape_id, old_lba, new_lba)) => {
            // Emit HeadPositionChanged event
            let _ = event_tx.send(TapeEvent::HeadPositionChanged {
                tape_id,
                old_lba,
                new_lba,
                reason: core_mediachanger::PositionChangeReason::Locate,
            });
            Ok(ScsiResp::good())
        }
        Err(e) => {
            tracing::warn!("LOCATE(10) failed: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

pub fn handle_locate_16(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;
    let event_tx = ctx.event_tx;

    // LOCATE(16) — same as LOCATE(10) but with an 8-byte target LBA
    // in cdb[4..12]. The CP bit lives in cdb[1] bit 1 and the
    // partition number in cdb[3].
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::good());
    }
    let cp = (cdb[1] & 0x02) != 0;
    let partition = cdb[3];
    let target_lba = u64::from_be_bytes([
        cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9], cdb[10], cdb[11],
    ]);
    match drive_manager.with_drive(drive_id, tsih, |cart| {
        let old_lba = cart.head_lba();
        let result = if cp {
            cart.locate_partition(partition, target_lba)
        } else {
            cart.locate(target_lba)
        };
        result.map(|()| (cart.label().to_string(), old_lba, cart.head_lba()))
    }) {
        Ok((tape_id, old_lba, new_lba)) => {
            let _ = event_tx.send(TapeEvent::HeadPositionChanged {
                tape_id,
                old_lba,
                new_lba,
                reason: core_mediachanger::PositionChangeReason::Locate,
            });
            Ok(ScsiResp::good())
        }
        Err(e) => {
            tracing::warn!("LOCATE(16) failed: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

pub fn handle_erase_6(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;

    // ERASE (6) — wipes the tape. The LONG bit (cdb[1] bit 0) is
    // ignored; we do a full erase regardless. The IMMED bit (bit 1)
    // is also ignored — the operation is fast on a virtual tape.
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::check_condition());
    }
    match drive_manager.with_drive(drive_id, tsih, |cart| cart.erase()) {
        Ok(()) => Ok(ScsiResp::good()),
        Err(e) => {
            tracing::warn!("ERASE(6) failed: {}", e);
            Ok(ScsiResp::check_condition_for(&e))
        }
    }
}

pub fn handle_set_capacity(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;

    // SET CAPACITY (SSC-5 §7.13). 6-byte CDB:
    //   byte 0:    opcode 0x0B
    //   byte 1:    bit 0 = IMMED (ignored — operation is fast on a
    //              virtual tape), bits 7..1 reserved
    //   bytes 2-3: CAPACITY PROPORTION VALUE (16-bit BE) — the
    //              fraction of native capacity to make available;
    //              65535 means full native, 0 is reserved (treat as
    //              full native).
    //
    // Per spec the operation is destructive: "all data on the medium
    // is destroyed" and the head is repositioned to BOM. We persist
    // the proportion in the cartridge manifest so EW/EOM gates fire
    // at the host-set effective capacity on subsequent writes; the
    // SET CAPACITY itself also erases the cartridge.
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::check_condition());
    }
    let proportion = u16::from_be_bytes([cdb[2], cdb[3]]);
    tracing::debug!(
        "SET CAPACITY (LUN {}, drive {}): proportion={} ({:.1}% of native)",
        lun,
        drive_id,
        proportion,
        if proportion == 0 {
            100.0
        } else {
            (proportion as f32 / 65535.0) * 100.0
        }
    );
    match drive_manager.with_drive(drive_id, tsih, |cart| {
        cart.set_capacity_proportion(proportion)
    }) {
        Ok(()) => Ok(ScsiResp::good()),
        Err(e) => {
            tracing::warn!("SET CAPACITY failed: {}", e);
            Ok(ScsiResp::check_condition_for(&e))
        }
    }
}

pub fn handle_read_6(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;
    let event_tx = ctx.event_tx;

    // READ(6)
    if ctx.is_changer_lun() {
        // Medium changer - no read operation
        return Ok(ScsiResp::check_condition());
    }
    // Variable-block-mode transfer length (FIXED bit ignored — every
    // READ(6) is currently treated as one logical block; cdb[2..5] is
    // the requested byte count). Needed for the SSC-4 §7.6 filemark
    // residual: host's allocated length minus what we returned.
    let xfer_len = ((ctx.cdb[2] as u32) << 16) | ((ctx.cdb[3] as u32) << 8) | (ctx.cdb[4] as u32);
    let mut data_out = vec![];
    let mut is_filemark = false;
    match drive_manager.with_drive(drive_id, tsih, |cart| {
        let lba_before = cart.head_lba();
        match cart.read_next() {
            Ok(blk) => {
                is_filemark = matches!(blk.kind, core_mediachanger::BlockKind::Filemark);
                data_out = if blk.data.is_empty() {
                    vec![]
                } else {
                    blk.data.to_vec()
                };
                // Return info for event emission
                Ok((
                    cart.label().to_string(),
                    cart.current_chunk_id(),
                    lba_before,
                ))
            }
            Err(e) => Err(e),
        }
    }) {
        Ok((tape_id, chunk_id, lba)) => {
            // Emit BlockRead event
            let _ = event_tx.send(TapeEvent::BlockRead {
                tape_id,
                chunk_id,
                lba,
            });
            // Filemark detection (SSC-4 §7.6): when READ(6) lands on a
            // filemark block, the drive returns CHECK CONDITION with
            // NO SENSE, FM=1, and INFO = residual (allocated bytes
            // minus what was transferred — here, the host's full
            // requested length since the filemark has zero data).
            // Without this sense the Linux st driver doesn't know it
            // crossed a filemark; the iSCSI layer sends back an empty
            // Data-In with status GOOD, and the host's read buffer is
            // left holding whatever was in it (which is what caused
            // the post-`mt eod` corruption probed in issue #25).
            if is_filemark {
                let sense = scsi::sense::SenseDataBuilder::new(
                    scsi::sense::SenseKey::NoSense,
                    scsi::sense::ASC_FILEMARK_DETECTED,
                )
                .with_filemark()
                .with_information(xfer_len)
                .build();
                return Ok(ScsiResp::check_condition_with_sense(sense));
            }
            // Logical Block Protection (LTO-7+): if RDPROTECT field
            // (CDB byte 1 bits 7..5) is non-zero AND the drive's Mode
            // Page 0x0A/0xF0 LBP_R bit is set, append a 4-byte
            // CRC32C trailer to the response. We compute fresh from
            // the just-read plaintext — BLAKE3 chunk hashes + AES-GCM
            // already prove the plaintext bytes are the originals, so
            // there is no separate stored CRC to compare against.
            let rdprotect = (ctx.cdb[1] >> 5) & 0x07;
            let (_, lbp_read_enabled) = drive_manager.lbp_enables(drive_id);
            if rdprotect != 0 && lbp_read_enabled && !data_out.is_empty() {
                let trailer = core_mediachanger::lbp::compute_lbp_trailer(&data_out);
                data_out.extend_from_slice(&trailer);
            }
            Ok(ScsiResp {
                status: ScsiStatus::Good,
                data_out,
                sense: None,
            })
        }
        Err(core_mediachanger::errors::SmcError::EndOfData) => {
            // Past-EOD READ(6) (SSC-4 §4.2.20 + 8.3.1). Return CHECK
            // CONDITION + BLANK CHECK + ASC/ASCQ 0x00/0x05 with INFO
            // = TRANSFER LENGTH — the host's full allocation is the
            // residual since zero data bytes were transferred.
            // Without the INFO field (and its VALID bit) the Linux
            // st driver can't compute the short-read count and dd's
            // userspace buffer comes back holding whatever stale
            // bytes its kernel page started with — the `14 00 00 08
            // ...` garbage in issue #26's reproducer. The EOM bit is
            // *not* set: end-of-data is not physical end-of-medium
            // (SSC-4 reserves EOM for VolumeOverflow / EarlyWarning).
            let sense = scsi::sense::SenseDataBuilder::new(
                scsi::sense::SenseKey::BlankCheck,
                scsi::sense::ASC_EOD_DETECTED,
            )
            .with_information(xfer_len)
            .build();
            Ok(ScsiResp::check_condition_with_sense(sense))
        }
        Err(e) => {
            tracing::warn!("READ(6) failed: {}", e);
            Ok(ScsiResp::check_condition_for(&e))
        }
    }
}

pub fn handle_write_6(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let lun = ctx.lun;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;
    let event_tx = ctx.event_tx;

    // WRITE(6) — immediate data only
    if ctx.is_changer_lun() {
        // Medium changer - no write operation
        tracing::warn!("WRITE(6): Cannot write to Medium Changer (LUN 0)");
        return Ok(ScsiResp::check_condition());
    }
    if ctx.pdu.data.is_empty() {
        tracing::warn!("WRITE(6): Empty data (pdu.data is empty)");
        // INVALID FIELD IN CDB — host sent zero-length WRITE.
        return Ok(ScsiResp::check_condition_for(
            &core_mediachanger::errors::SmcError::InvalidField,
        ));
    }
    // Drive-side write-mode constraints (Append-Only / Encrypt-Only
    // from saved Mode Page 0x10/0x01). Cheap no-op when neither is
    // active.
    if let Err(e) = drive_manager.enforce_write_mode(drive_id, tsih) {
        tracing::warn!("WRITE(6) refused by drive write-mode: {}", e);
        return Ok(ScsiResp::check_condition_for(&e));
    }
    tracing::debug!(
        "WRITE(6): LUN={}, drive_id={}, data_len={} bytes",
        lun,
        drive_id,
        ctx.pdu.data.len()
    );
    // Logical Block Protection: WRPROTECT field (CDB byte 1 bits 7..5)
    // declares the host has appended a 4-byte CRC32C trailer. We
    // validate, strip, and pass the data forward; mismatch → CHECK
    // CONDITION + ABORTED COMMAND + 0x10/0x05. WRPROTECT is only
    // honored when the drive's Mode Page 0x0A/0xF0 LBP_W enable is
    // non-zero — otherwise we ignore the CDB bits and treat the bytes
    // as plain data (matches LBP-off real-LTO behavior).
    let wrprotect = (ctx.cdb[1] >> 5) & 0x07;
    let (lbp_write_enabled, _) = drive_manager.lbp_enables(drive_id);
    if wrprotect != 0 && lbp_write_enabled {
        match core_mediachanger::lbp::strip_and_validate_lbp(&ctx.pdu.data) {
            Ok(stripped) => {
                let stripped = stripped.to_vec();
                ctx.pdu.data = stripped;
            }
            Err(e) => {
                tracing::warn!(
                    "WRITE(6) LBP CRC32C validation failed (WRPROTECT={}): {}",
                    wrprotect,
                    e
                );
                let sense = scsi::sense::SenseDataBuilder::new(
                    scsi::sense::SenseKey::AbortedCommand,
                    scsi::sense::ASC_LOGICAL_BLOCK_PROTECTION_METHOD_ERROR,
                )
                .build();
                return Ok(ScsiResp::check_condition_with_sense(sense));
            }
        }
    }
    // Take ownership of the PDU's payload Vec — `Bytes::from(Vec)`
    // is zero-copy (wraps the existing allocation). The earlier
    // `copy_from_slice` allocated a fresh buffer and memcpy'd
    // every WRITE block through CPU just to satisfy the type.
    let data_to_write = bytes::Bytes::from(std::mem::take(&mut ctx.pdu.data));
    let data_len = data_to_write.len() as u64;

    // Attempt to write data and handle errors properly
    match drive_manager.with_drive(drive_id, tsih, |cart| {
        tracing::debug!(
            "WRITE(6): Calling cart.write_data() for {} bytes",
            data_to_write.len()
        );
        let lba_before = cart.next_lba();
        cart.write_data(data_to_write.clone())?;
        tracing::debug!(
            "WRITE(6): Successfully wrote {} bytes to drive {}",
            data_to_write.len(),
            drive_id
        );

        // Return info for event emission
        Ok((
            cart.label().to_string(),
            cart.current_chunk_id(),
            lba_before,
        ))
    }) {
        Ok((tape_id, chunk_id, lba)) => {
            // Emit BlockWritten event
            let _ = event_tx.send(TapeEvent::BlockWritten {
                tape_id,
                chunk_id,
                lba,
                size: data_len,
            });

            tracing::debug!("WRITE(6): SCSI status GOOD");
            Ok(ScsiResp::good())
        }
        Err(e) => {
            tracing::warn!("WRITE(6) failed: {}", e);
            Ok(ScsiResp::check_condition_for(&e))
        }
    }
}

pub fn handle_verify_6(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;

    // VERIFY (6, SSC) — re-read N blocks and validate their stored
    // BLAKE3 checksum. We don't compare against host-provided data
    // (BYTCMP=1 path is unsupported); BYTCMP=0 means medium-only
    // verify, which is exactly what scrub_all gives us at the
    // single-block level via verify_block.
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::check_condition());
    }
    let count = ((cdb[2] as u32) << 16) | ((cdb[3] as u32) << 8) | (cdb[4] as u32);
    match drive_manager.with_drive(drive_id, tsih, |cart| {
        for _ in 0..count {
            if cart.at_eod() {
                break;
            }
            let lba = cart.head_lba();
            // verify_block re-reads the chunk and re-checks BLAKE3.
            cart.verify_block(lba)?;
            cart.locate(lba + 1)?;
        }
        Ok(())
    }) {
        Ok(()) => Ok(ScsiResp::good()),
        Err(e) => {
            tracing::warn!("VERIFY(6) error: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

pub fn handle_verify_16(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;

    // VERIFY (16, SSC) — same as VERIFY(6) but with an 8-byte count
    // in cdb[4..12]. LTO-7+ uses this for tapes larger than 16M
    // blocks of any kind.
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::check_condition());
    }
    let count = u64::from_be_bytes([
        cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9], cdb[10], cdb[11],
    ]);
    match drive_manager.with_drive(drive_id, tsih, |cart| {
        for _ in 0..count {
            if cart.at_eod() {
                break;
            }
            let lba = cart.head_lba();
            cart.verify_block(lba)?;
            cart.locate(lba + 1)?;
        }
        Ok(())
    }) {
        Ok(()) => Ok(ScsiResp::good()),
        Err(e) => {
            tracing::warn!("VERIFY(16) error: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

pub fn handle_prevent_allow_medium_removal(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;

    // PREVENT/ALLOW MEDIUM REMOVAL (SPC-4 §6.13). cdb[4] bits 1-0 are
    // independent prevent flags:
    //   bit 0 (data transport) — gates SCSI UNLOAD on this drive and
    //                            MOVE MEDIUM with this drive as source.
    //   bit 1 (mechanical)     — gates the admin
    //                            `POST /api/v1/changer/unload` endpoint
    //                            (the operator-console analog of the
    //                            front-panel eject button). `force: true`
    //                            on the admin request overrides.
    //                            See drive_manager::is_mechanical_prevented.
    let bits = drive_manager::PreventBits {
        data_transport: (cdb[4] & 0x01) != 0,
        mechanical: (cdb[4] & 0x02) != 0,
    };

    // Changer LUN: thurvtl has no portal door, and import/export are
    // out-of-band CLI ops that require the daemon stopped. Accept the
    // command (faithful to SPC-4 — every LUN must accept it) but don't
    // track state since there's nothing to gate on.
    if ctx.is_changer_lun() {
        tracing::debug!(
            "PREVENT/ALLOW MEDIUM REMOVAL on changer LUN: data_transport={} mechanical={} (no enforcement target)",
            bits.data_transport,
            bits.mechanical
        );
        return Ok(ScsiResp::good());
    }

    if let Err(e) = drive_manager.set_prevent(drive_id, tsih, bits) {
        tracing::warn!("PREVENT/ALLOW MEDIUM REMOVAL: {}", e);
        return Ok(ScsiResp::check_condition_for(&e));
    }

    tracing::debug!(
        "PREVENT/ALLOW MEDIUM REMOVAL: LUN={} drive={} TSIH={} data_transport={} mechanical={}",
        lun,
        drive_id,
        tsih,
        bits.data_transport,
        bits.mechanical
    );
    Ok(ScsiResp::good())
}

pub fn handle_allow_overwrite(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;

    // ALLOW OVERWRITE (SSC §7.2). Sets a per-partition LBA past
    // which writes overwrite (rather than truncate) what's
    // already on the medium. LTFS uses this to append a fresh
    // index record to P0 without losing the prior chain.
    //
    // CDB layout (16 bytes):
    //   cdb[2]: ALLOW OVERWRITE field (0=disabled, 1=current pos, 2=enabled)
    //   cdb[3]: partition number
    //   cdb[4..12]: LBA (8 bytes BE)
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::check_condition());
    }
    let allow_field = cdb[2] & 0x0F;
    let partition = cdb[3];
    let lba = u64::from_be_bytes([
        cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9], cdb[10], cdb[11],
    ]);
    tracing::info!(
        "ALLOW OVERWRITE: allow=0x{:02x}, partition={}, lba={}",
        allow_field,
        partition,
        lba
    );
    let result = drive_manager.with_drive(drive_id, tsih, |cart| match allow_field {
        // 0x00 — disable barrier on the partition.
        0x00 => cart.set_allow_overwrite(partition, 0),
        // 0x01 — barrier at current head position. We use
        // the head_lba of the active partition; LTFS only
        // ever issues ALLOW OVERWRITE on the active one.
        0x01 => {
            let head = cart.head_lba();
            cart.set_allow_overwrite(partition, head)
        }
        // 0x02 — barrier at the supplied LBA.
        0x02 => cart.set_allow_overwrite(partition, lba),
        _ => Err(core_mediachanger::errors::SmcError::InvalidOp(
            "ALLOW OVERWRITE: unsupported field",
        )),
    });
    match result {
        Ok(()) => Ok(ScsiResp::good()),
        Err(e) => {
            tracing::warn!("ALLOW OVERWRITE failed: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

pub fn handle_format_medium(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;

    // FORMAT MEDIUM (SSC §7.1). The FORMAT field in cdb[2] selects:
    //   0x00 — default format (erase, keep current partition layout)
    //   0x01 — apply pending Mode Page 0x11 layout (LTFS partitioning)
    //   0x02 — default partition (revert to single partition)
    // mkltfs issues MODE SELECT page 0x11 then FORMAT MEDIUM 0x01.
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::check_condition());
    }
    let format_field = cdb[2] & 0x0F;
    tracing::info!("FORMAT MEDIUM: format=0x{:02x}", format_field);
    match drive_manager.with_drive(drive_id, tsih, |cart| {
        cart.apply_format_medium(format_field)
    }) {
        Ok(()) => Ok(ScsiResp::good()),
        Err(e) => {
            tracing::warn!("FORMAT MEDIUM failed: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

pub fn handle_read_attribute(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;
    let drive_manager = ctx.drive_manager;

    // READ ATTRIBUTE
    if ctx.is_changer_lun() {
        // Medium changer - not implemented
        return Ok(ScsiResp::check_condition());
    }
    let alloc = u32::from_be_bytes([cdb[10], cdb[11], cdb[12], cdb[13]]);
    let service_action = cdb[1] & 0x1F;
    let element_address = u16::from_be_bytes([cdb[8], cdb[9]]);
    let first_attribute = u16::from_be_bytes([cdb[6], cdb[7]]);

    tracing::debug!(
        "READ ATTRIBUTE: LUN={}, SA=0x{:02x}, element={}, first_attr=0x{:04x}, alloc={}",
        lun,
        service_action,
        element_address,
        first_attribute,
        alloc
    );

    // Build MAM info from the loaded cartridge (label + real capacity)
    let cartridge_label = drive_manager
        .get_cartridge_label((lun - 1) as usize)
        .ok()
        .flatten();
    let cartridge_capacity = drive_manager.get_cartridge_capacity((lun - 1) as usize);
    let mam_info = match (cartridge_label.as_deref(), cartridge_capacity) {
        (Some(label), Some((max_bytes, remaining_bytes))) => {
            Some(scsi::attributes::CartridgeMamInfo {
                label,
                max_capacity_bytes: max_bytes,
                remaining_capacity_bytes: remaining_bytes,
            })
        }
        _ => None,
    };

    match scsi::attributes::handle_read_attribute(
        service_action,
        element_address,
        first_attribute,
        mam_info,
    ) {
        Ok(data) => {
            tracing::debug!("READ ATTRIBUTE response: {} bytes", data.len());
            Ok(ScsiResp {
                status: ScsiStatus::Good,
                data_out: limit_len(data, alloc),
                sense: None,
            })
        }
        Err(e) => {
            tracing::warn!("READ ATTRIBUTE error: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

pub fn handle_write_attribute(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    // WRITE ATTRIBUTE — accept attribute writes from backup software.
    // We validate the parameter list shape but don't yet persist
    // attribute values into the cartridge manifest (most software
    // writes host-private metadata that doesn't need to round-trip).
    if ctx.is_changer_lun() {
        return Ok(ScsiResp::check_condition());
    }
    match scsi::attributes::handle_write_attribute(&ctx.pdu.data) {
        Ok(()) => Ok(ScsiResp::good()),
        Err(e) => {
            tracing::warn!("WRITE ATTRIBUTE error: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

pub fn handle_write_buffer(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    // WRITE BUFFER (SPC) — firmware download / drive log dump
    // upload. We accept the buffer and discard it. Backup-software
    // library-management tools poke this to confirm the path.
    tracing::debug!(
        "WRITE BUFFER: LUN={}, mode=0x{:02x}, {} bytes (discarded)",
        ctx.lun,
        ctx.cdb[1] & 0x1F,
        ctx.pdu.data.len()
    );
    Ok(ScsiResp::good())
}

pub fn handle_read_buffer(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;

    // READ BUFFER (SPC) — symmetric stub that returns the requested
    // number of zero bytes. Real drives return current firmware /
    // dump data here.
    let alloc = u32::from_be_bytes([0, cdb[6], cdb[7], cdb[8]]);
    tracing::debug!(
        "READ BUFFER: LUN={}, mode=0x{:02x}, alloc={}",
        lun,
        cdb[1] & 0x1F,
        alloc
    );
    let d = vec![0u8; alloc.min(4096) as usize];
    Ok(ScsiResp {
        status: ScsiStatus::Good,
        data_out: d,
        sense: None,
    })
}

// RESERVE(6/10) / RELEASE(6/10) — accept-and-GOOD on every LUN.
// Both products are single-initiator-per-LUN by construction, so
// the classic "third-party reservation" semantics don't apply:
// there's no second initiator to lock out and no shared state to
// protect. We accept the CDB as a no-op for compatibility with
// the SCSI reservation handshake some backup software does at
// session start (RESERVE / RELEASE accepted on any LUN).

pub fn handle_reserve_6(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    tracing::debug!("RESERVE(6) LUN={} - accepted (no-op)", ctx.lun);
    Ok(ScsiResp::good())
}

pub fn handle_release_6(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    tracing::debug!("RELEASE(6) LUN={} - accepted (no-op)", ctx.lun);
    Ok(ScsiResp::good())
}

pub fn handle_reserve_10(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    tracing::debug!("RESERVE(10) LUN={} - accepted (no-op)", ctx.lun);
    Ok(ScsiResp::good())
}

pub fn handle_release_10(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    tracing::debug!("RELEASE(10) LUN={} - accepted (no-op)", ctx.lun);
    Ok(ScsiResp::good())
}

/// REPORT LUNS (SPC-4) — enumerate the LUNs this device exposes.
/// Pulls in-partition drive ids from the facade, then emits LUN 0
/// (changer) plus each drive at LUN drive_id+1. Partition fencing
/// happens at the facade so out-of-partition drives are hidden from
/// the initiator's discovery.
pub fn handle_report_luns(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let alloc = u32::from_be_bytes([cdb[6], cdb[7], cdb[8], cdb[9]]);
    tracing::debug!("REPORT LUNS: allocation_length={}", alloc);

    let drive_ids = ctx.facade.drive_ids_in_partition(ctx.session_partition);
    let mut luns_u64: Vec<u64> = Vec::with_capacity(drive_ids.len() + 1);
    luns_u64.push(0); // LUN 0 = changer
    luns_u64.extend(drive_ids.into_iter().map(|d| u64::from(d) + 1));
    let d = scsi_spc::report_luns::build_report_luns(&luns_u64);

    tracing::debug!(
        "REPORT LUNS response: {} bytes ({} LUNs, partition={:?})",
        d.len(),
        luns_u64.len(),
        ctx.session_partition,
    );
    Ok(ScsiResp {
        status: ScsiStatus::Good,
        data_out: limit_len(d, alloc),
        sense: None,
    })
}

/// Drive-LUN LOG SENSE (SPC-4 §6.6 / SSC-3) — facade-backed because
/// LOG SENSE page `0x14` parameter `0x0040` reports the drive
/// manufacturer serial and must agree with INQUIRY VPD `0xB1`. The
/// changer LUN's LOG SENSE response (Supported / Temperature /
/// TapeAlert only) stays in thurvtl — the shared dispatcher
/// only ever runs for drive LUNs.
pub fn handle_log_sense(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;
    let drive_id = ctx.drive_id;

    let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as u32;
    let page_code = cdb[2] & 0x3F;
    let subpage_code = cdb[3];
    let pc = (cdb[2] >> 6) & 0x03;

    tracing::debug!(
        "LOG SENSE: LUN={}, page_code=0x{:02x}, subpage=0x{:02x}, PC={}, alloc={}",
        lun,
        page_code,
        subpage_code,
        pc,
        alloc
    );

    let mfg_serial = ctx
        .facade
        .drive_mfg_serial(drive_id as u32)
        .unwrap_or_else(|| drive_mfg_serial_fallback(lun));

    match scsi::log_pages::handle_log_sense(page_code, subpage_code, pc, &mfg_serial) {
        Ok(data) => {
            tracing::debug!("LOG SENSE response: {} bytes", data.len());
            Ok(ScsiResp {
                status: ScsiStatus::Good,
                data_out: limit_len(data, alloc),
                sense: None,
            })
        }
        Err(e) => {
            tracing::warn!("LOG SENSE error: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

/// SECURITY PROTOCOL OUT (SPC) — drive-LUN LTO encryption. CDB 0xB5
/// is overloaded with REQUEST VOLUME ELEMENT ADDRESS on the changer,
/// so thurvtl's dispatch shell routes by LUN before reaching
/// here.
///
/// Accepts protocol 0x20 / SPSP 0x0010 (Set Data Encryption); installs
/// or clears the per-drive encryption state on the loaded cartridge.
/// NOTE: never log the key bytes — only metadata.
pub fn handle_security_protocol_out(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;
    let audit_log = ctx.audit_log;
    let audit_ratelimiter = ctx.audit_ratelimiter;

    let security_protocol = cdb[1];
    let spsp = u16::from_be_bytes([cdb[2], cdb[3]]);
    if security_protocol != scsi::encryption_pages::SECURITY_PROTOCOL_TAPE_DATA_ENC {
        tracing::warn!(
            "SECURITY PROTOCOL OUT: unsupported protocol 0x{:02x}",
            security_protocol
        );
        return Ok(ScsiResp::check_condition());
    }
    if spsp != scsi::encryption_pages::PAGE_SET_DATA_ENCRYPTION {
        tracing::warn!(
            "SECURITY PROTOCOL OUT: unsupported SPSP 0x{:04x} for protocol 0x20",
            spsp
        );
        return Ok(ScsiResp::check_condition());
    }
    match scsi::encryption_pages::parse_set_data_encryption(&ctx.pdu.data) {
        Ok(scsi::encryption_pages::SetDataEncryption::SetKey(state)) => {
            let algorithm_index = state.algorithm_index;
            let res = drive_manager.with_drive(drive_id, tsih, |cart| {
                cart.set_encryption_state(state.clone());
                Ok(())
            });
            match res {
                Ok(()) => {
                    tracing::info!(
                        "SET DATA ENCRYPTION: drive {} key installed (algo idx {})",
                        drive_id,
                        algorithm_index
                    );
                    audit_append(
                        audit_log,
                        audit_ratelimiter,
                        "iscsi.encryption.set_key",
                        ctx.audit_actor(),
                        serde_json::json!({
                            "drive": drive_id,
                            "lun": lun,
                            "algorithm_index": algorithm_index,
                        }),
                        AuditResult::Ok,
                    );
                    Ok(ScsiResp::good())
                }
                Err(e) => {
                    tracing::warn!("SET DATA ENCRYPTION install failed: {}", e);
                    audit_append(
                        audit_log,
                        audit_ratelimiter,
                        "iscsi.encryption.set_key",
                        ctx.audit_actor(),
                        serde_json::json!({
                            "drive": drive_id,
                            "lun": lun,
                            "algorithm_index": algorithm_index,
                        }),
                        AuditResult::Error(e.to_string()),
                    );
                    Ok(ScsiResp::check_condition())
                }
            }
        }
        Ok(scsi::encryption_pages::SetDataEncryption::Clear) => {
            let res = drive_manager.with_drive(drive_id, tsih, |cart| {
                cart.clear_encryption();
                Ok(())
            });
            match res {
                Ok(()) => {
                    tracing::info!("SET DATA ENCRYPTION: drive {} key cleared", drive_id);
                    audit_append(
                        audit_log,
                        audit_ratelimiter,
                        "iscsi.encryption.clear_key",
                        ctx.audit_actor(),
                        serde_json::json!({
                            "drive": drive_id,
                            "lun": lun,
                        }),
                        AuditResult::Ok,
                    );
                    Ok(ScsiResp::good())
                }
                Err(e) => {
                    tracing::warn!("SET DATA ENCRYPTION clear failed: {}", e);
                    audit_append(
                        audit_log,
                        audit_ratelimiter,
                        "iscsi.encryption.clear_key",
                        ctx.audit_actor(),
                        serde_json::json!({
                            "drive": drive_id,
                            "lun": lun,
                        }),
                        AuditResult::Error(e.to_string()),
                    );
                    Ok(ScsiResp::check_condition())
                }
            }
        }
        Err(reason) => {
            tracing::warn!("SET DATA ENCRYPTION parse error: {}", reason);
            Ok(ScsiResp::check_condition())
        }
    }
}

/// SECURITY PROTOCOL IN (SPC) — drive-LUN encryption probe. Reports
/// supported protocols (`security_protocol == 0x00`) and the Tape
/// Data Encryption page family on protocol 0x20. thurvtl's
/// dispatch routes this only on drive LUNs (the changer rejects).
pub fn handle_security_protocol_in(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;

    let alloc = u32::from_be_bytes([cdb[6], cdb[7], cdb[8], cdb[9]]);
    let security_protocol = cdb[1];
    let spsp = u16::from_be_bytes([cdb[2], cdb[3]]);
    tracing::debug!(
        "SECURITY PROTOCOL IN: protocol=0x{:02x}, spsp=0x{:04x}, alloc={}",
        security_protocol,
        spsp,
        alloc
    );

    if security_protocol == 0x00 {
        let d = scsi::encryption_pages::build_supported_protocols();
        return Ok(ScsiResp {
            status: ScsiStatus::Good,
            data_out: limit_len(d, alloc),
            sense: None,
        });
    }

    if security_protocol != scsi::encryption_pages::SECURITY_PROTOCOL_TAPE_DATA_ENC
        || ctx.is_changer_lun()
    {
        tracing::warn!(
            "SECURITY PROTOCOL IN: unsupported protocol 0x{:02x} on LUN {}",
            security_protocol,
            lun
        );
        return Ok(ScsiResp::check_condition());
    }

    let d = match spsp {
        scsi::encryption_pages::PAGE_TAPE_DATA_ENC_IN_SUPPORT => {
            scsi::encryption_pages::build_in_support_page()
        }
        scsi::encryption_pages::PAGE_TAPE_DATA_ENC_OUT_SUPPORT => {
            scsi::encryption_pages::build_out_support_page()
        }
        scsi::encryption_pages::PAGE_DATA_ENCRYPTION_CAPABILITIES => {
            scsi::encryption_pages::build_capabilities_page()
        }
        scsi::encryption_pages::PAGE_SUPPORTED_KEY_FORMATS => {
            scsi::encryption_pages::build_supported_key_formats_page()
        }
        scsi::encryption_pages::PAGE_DATA_ENCRYPTION_STATUS => {
            let res = drive_manager.with_drive(drive_id, tsih, |cart| {
                Ok(scsi::encryption_pages::build_encryption_status_page(
                    cart.encryption_state(),
                ))
            });
            match res {
                Ok(page) => page,
                Err(_) => scsi::encryption_pages::build_encryption_status_page(None),
            }
        }
        scsi::encryption_pages::PAGE_NEXT_BLOCK_ENCRYPTION_STATUS => {
            let res = drive_manager.with_drive(drive_id, tsih, |cart| {
                Ok(scsi::encryption_pages::build_next_block_status_page(
                    cart.head_lba(),
                    cart.next_block_is_encrypted(),
                    cart.next_block_algorithm_index(),
                ))
            });
            match res {
                Ok(page) => page,
                Err(_) => scsi::encryption_pages::build_next_block_status_page(0, false, 0),
            }
        }
        _ => {
            tracing::warn!(
                "SECURITY PROTOCOL IN: unknown SPSP 0x{:04x} for protocol 0x20",
                spsp
            );
            return Ok(ScsiResp::check_condition());
        }
    };
    Ok(ScsiResp {
        status: ScsiStatus::Good,
        data_out: limit_len(d, alloc),
        sense: None,
    })
}

/// MAINTENANCE OUT (SPC). Service action 0x0F = SET TIMESTAMP and
/// 0x1E = WRITE DYNAMIC RUNTIME ATTRIBUTE — both accepted-and-discarded
/// (we tag events with our own clock, model no tunable attributes).
pub fn handle_maintenance_out(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;

    let service_action = cdb[1] & 0x1F;
    match service_action {
        0x0F => {
            tracing::debug!(
                "SET TIMESTAMP: parameter list = {} bytes (discarded)",
                ctx.pdu.data.len()
            );
            Ok(ScsiResp::good())
        }
        0x1E => {
            // WRITE DYNAMIC RUNTIME ATTRIBUTE — vendor-specific runtime
            // tunables (drive performance mode, LED behavior,
            // power-management knobs). Virtual drives have no internal
            // tunables — accept and discard, matching how SET TIMESTAMP
            // is handled. Refusing would surface as a capability gap to
            // backup-software probes; READ DRA returns an empty list.
            tracing::debug!(
                "WRITE DYNAMIC RUNTIME ATTRIBUTE: parameter list = {} bytes (discarded - no tunables modeled)",
                ctx.pdu.data.len()
            );
            Ok(ScsiResp::good())
        }
        _ => {
            tracing::warn!(
                "MAINTENANCE OUT: unsupported service action 0x{:02x}",
                service_action
            );
            Ok(ScsiResp::check_condition())
        }
    }
}

/// PERSISTENT RESERVE IN (SPC §6.13). We don't implement real
/// multi-host clustering, but we answer the standard service actions
/// (0x00 READ KEYS, 0x01 READ RESERVATION, 0x02 REPORT CAPABILITIES,
/// 0x03 READ FULL STATUS) so backup-software probe paths don't error.
pub fn handle_persistent_reserve_in(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;

    let service_action = cdb[1] & 0x1F;
    let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as u32;
    let d = match service_action {
        0x00 | 0x03 => {
            // 8-byte header: PRgeneration (4) + additional length (4) = 0
            vec![0u8; 8]
        }
        0x01 => {
            // 8-byte header: PRgeneration (4) + additional length (4) = 0
            vec![0u8; 8]
        }
        0x02 => {
            // REPORT CAPABILITIES: 8-byte response, all flags clear,
            // no persistent-through-power-loss support.
            let mut buf = vec![0u8; 8];
            buf[0] = 0x00;
            buf[1] = 0x08; // length
            buf
        }
        _ => {
            tracing::warn!(
                "PERSISTENT RESERVE IN: unsupported SA 0x{:02x}",
                service_action
            );
            return Ok(ScsiResp::check_condition());
        }
    };
    Ok(ScsiResp {
        status: ScsiStatus::Good,
        data_out: limit_len(d, alloc),
        sense: None,
    })
}

/// PERSISTENT RESERVE OUT (SPC §6.14). We deliberately refuse —
/// clustering-aware backup software (Veeam HA, NetBackup MSDP
/// cluster, etc.) downgrades to single-host mode safely instead of
/// silently believing it has acquired exclusive access. PRIN (0x5E)
/// still reports "no reservations / no registrations / no
/// capabilities" — that's the truthful answer and lets probes complete.
pub fn handle_persistent_reserve_out(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let service_action = ctx.cdb[1] & 0x1F;
    tracing::warn!(
        "PERSISTENT RESERVE OUT rejected: SA=0x{:02x} (clustering not supported)",
        service_action
    );
    Ok(ScsiResp::check_condition())
}

/// MAINTENANCE IN (SPC §6.27). Service action picks the variant —
/// REPORT SUPPORTED OPCODES, READ DYNAMIC RUNTIME ATTRIBUTE, READ
/// LOGGED-IN HOST TABLE, REPORT SUPPORTED TASK MGMT FUNCTIONS, REPORT
/// TARGET PORT GROUPS (ALUA), REPORT TIMESTAMP.
pub fn handle_maintenance_in(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;

    let service_action = cdb[1] & 0x1F;
    let alloc = u32::from_be_bytes([cdb[6], cdb[7], cdb[8], cdb[9]]);
    match service_action {
        0x0C => {
            // REPORT SUPPORTED OPCODES — per-LUN list of every CDB
            // this LUN dispatches. Keep in sync with the dispatch
            // arms in thurvtl.
            let opcodes_changer: &[u8] = &[
                0x00, 0x03, 0x07, 0x12, 0x15, 0x16, 0x17, 0x1A, 0x1C, 0x1D, 0x1E, 0x2B, 0x37, 0x3B,
                0x3C, 0x4C, 0x4D, 0x55, 0x56, 0x57, 0x5A, 0x5E, 0xA0, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6,
                0xB5, 0xB6, 0xB8,
            ];
            // PERSISTENT RESERVE OUT (0x5F) is intentionally absent —
            // the daemon rejects it with CHECK CONDITION rather than
            // claiming partial support. PRIN (0x5E) is advertised
            // because it returns truthful "no reservations" responses.
            // RESERVE/RELEASE (6/10) and SEND VOLUME TAG are accepted
            // as no-ops for backup-software compatibility — see
            // docs/SPEC.md "Reservations" and "SEND VOLUME TAG".
            let opcodes_tape: &[u8] = &[
                0x00, 0x01, 0x03, 0x04, 0x05, 0x08, 0x0A, 0x0B, 0x10, 0x11, 0x12, 0x13, 0x15, 0x19,
                0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x2B, 0x34, 0x3B, 0x3C, 0x44, 0x4C, 0x4D, 0x55, 0x5A,
                0x5E, 0x80, 0x82, 0x8C, 0x8D, 0x8F, 0x91, 0x92, 0xA0, 0xA2, 0xA3, 0xA4, 0xB5,
            ];
            let opcodes: &[u8] = if ctx.is_changer_lun() {
                opcodes_changer
            } else {
                opcodes_tape
            };

            let mut data = Vec::with_capacity(4 + opcodes.len() * 8);
            data.extend_from_slice(&[0u8; 4]); // length placeholder
            for &op in opcodes {
                let mut entry = [0u8; 8];
                entry[0] = op;
                entry[6] = 0xFF;
                entry[7] = 0xFF;
                data.extend_from_slice(&entry);
            }
            let payload_len = (data.len() - 4) as u32;
            data[0..4].copy_from_slice(&payload_len.to_be_bytes());
            Ok(ScsiResp {
                status: ScsiStatus::Good,
                data_out: limit_len(data, alloc),
                sense: None,
            })
        }
        0x1E => {
            // READ DYNAMIC RUNTIME ATTRIBUTE (MAINTENANCE IN
            // SA 0x1E). Virtual drives have no
            // internal tunables — return a well-formed empty parameter
            // list (header only, length=0). Backup-software capability
            // probes parse this as "drive supports the page but exposes
            // no tunable attributes". WRITE DRA (MAINTENANCE OUT SA
            // 0x1E) is accepted-and-discarded for symmetry.
            let data = vec![0u8; 4];
            Ok(ScsiResp {
                status: ScsiStatus::Good,
                data_out: limit_len(data, alloc),
                sense: None,
            })
        }
        0x1F => {
            // READ LOGGED-IN HOST TABLE (MAINTENANCE IN SA 0x1F).
            // Backup software uses this
            // to discover which initiators are currently logged in to
            // the drive — useful on FC where multiple hosts share a
            // target port. iSCSI is single-session-per-(target, IQN)
            // by RFC 3720, but multiple sessions to different IQNs can
            // still share a target portal. We're single-initiator-per-
            // LUN by construction, so we report exactly one descriptor
            // for the IQN of the session that issued the command. If
            // the IQN isn't known (CLI-injected synthetic SCSI,
            // smoke-test mode), the table is empty.
            //
            // Response layout:
            //   bytes 0..3   parameter data length (BE32, excludes
            //                these 4 bytes)
            //   per-host descriptor (256 bytes):
            //     bytes 0..1  descriptor length (BE16, = 254)
            //     bytes 2..3  reserved
            //     bytes 4..   initiator port name (252 ASCII bytes,
            //                 NUL-padded — fits the 223-byte IQN
            //                 maximum from RFC 3722)
            const DESC_LEN: usize = 256;
            let mut data = Vec::with_capacity(4 + DESC_LEN);
            data.extend_from_slice(&[0u8; 4]); // length placeholder

            if let Some(iqn) = ctx.initiator_iqn {
                let mut desc = vec![0u8; DESC_LEN];
                let payload_len = (DESC_LEN - 2) as u16;
                desc[0..2].copy_from_slice(&payload_len.to_be_bytes());
                let bytes = iqn.as_bytes();
                let copy_len = bytes.len().min(DESC_LEN - 4);
                desc[4..4 + copy_len].copy_from_slice(&bytes[..copy_len]);
                data.extend_from_slice(&desc);
            }

            let body_len = (data.len() - 4) as u32;
            data[0..4].copy_from_slice(&body_len.to_be_bytes());
            Ok(ScsiResp {
                status: ScsiStatus::Good,
                data_out: limit_len(data, alloc),
                sense: None,
            })
        }
        0x0D => {
            // REPORT SUPPORTED TASK MANAGEMENT FUNCTIONS (SPC-4 §6.27.4).
            // 4-byte response advertising which TMFs the target accepts.
            // The iSCSI session layer's TMF handler unconditionally
            // returns "Function complete" for every Task Management
            // Function Request — there are no outstanding tasks to
            // abort on a single-initiator-per-LUN virtual target — so
            // advertise the SAM-/iSCSI-standard set:
            //
            //   byte 0 bit 7 ATS    ABORT TASK
            //   byte 0 bit 6 ATSS   ABORT TASK SET
            //   byte 0 bit 4 CTSS   CLEAR TASK SET
            //   byte 0 bit 3 LURS   LOGICAL UNIT RESET
            //   byte 1 bit 7 ITNRS  I_T NEXUS RESET
            //
            // CACAS / QTS / QAES / QTSS not advertised — we don't model
            // ACA, query semantics, or async-event endpoints. WAKES
            // (byte 0 bit 0) is obsolete.
            let mut data = vec![0u8; 4];
            data[0] = 0x80 | 0x40 | 0x10 | 0x08; // ATS | ATSS | CTSS | LURS
            data[1] = 0x80; // ITNRS
            Ok(ScsiResp {
                status: ScsiStatus::Good,
                data_out: limit_len(data, alloc),
                sense: None,
            })
        }
        0x0A => {
            // REPORT TARGET PORT GROUPS (ALUA). Single port group,
            // single port, status = active/optimized. SPC-4 §6.27.7.
            //
            //   bytes 0..3   return data length (= 8+8 = 16 - 4 = 12)
            //   byte 4       PREF/asym access state (0x00 = optimized)
            //   byte 5       supported access states bitmap
            //   bytes 6..7   target port group (0)
            //   byte 8       reserved
            //   byte 9       status code
            //   byte 10      vendor specific
            //   byte 11      port count (= 1)
            //   bytes 12..15 port descriptor: reserved + relative target port id (0x0001)
            let mut data = vec![0u8; 16];
            data[0..4].copy_from_slice(&12u32.to_be_bytes());
            data[4] = 0x00; // active/optimized
            data[5] = 0x80; // ao_sup (active/optimized supported)
            data[6..8].copy_from_slice(&0u16.to_be_bytes()); // tpg id
            data[11] = 1; // 1 port
            data[14..16].copy_from_slice(&1u16.to_be_bytes()); // rel port id
            Ok(ScsiResp {
                status: ScsiStatus::Good,
                data_out: limit_len(data, alloc),
                sense: None,
            })
        }
        0x0F => {
            // REPORT TIMESTAMP (SPC-4 §6.27.10). 12-byte response:
            //   bytes 0..1   timestamp parameter data length (=10)
            //   byte 2       reserved | origin
            //   byte 3       reserved
            //   bytes 4..9   timestamp (48-bit milliseconds since
            //                           1970-01-01T00:00:00Z)
            //   bytes 10..11 reserved
            let now_ms: u64 = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let mut data = vec![0u8; 12];
            data[0..2].copy_from_slice(&10u16.to_be_bytes());
            data[2] = 0x02; // origin: timestamp set by SET TIMESTAMP
            let ts48 = now_ms & 0x0000_FFFF_FFFF_FFFFu64;
            data[4] = ((ts48 >> 40) & 0xFF) as u8;
            data[5] = ((ts48 >> 32) & 0xFF) as u8;
            data[6] = ((ts48 >> 24) & 0xFF) as u8;
            data[7] = ((ts48 >> 16) & 0xFF) as u8;
            data[8] = ((ts48 >> 8) & 0xFF) as u8;
            data[9] = (ts48 & 0xFF) as u8;
            Ok(ScsiResp {
                status: ScsiStatus::Good,
                data_out: limit_len(data, alloc),
                sense: None,
            })
        }
        _ => {
            tracing::warn!(
                "MAINTENANCE IN: unsupported service action 0x{:02x}",
                service_action
            );
            Ok(ScsiResp::check_condition())
        }
    }
}

/// Per-LUN partition fence. Sessions bound to a logical partition
/// (CHAP user → partition mapping) can only address drives that
/// partition owns; LUN 0 (the changer, when present) stays accessible
/// to every session — partition fencing for SMC ops happens at the
/// element level inside MOVE MEDIUM / READ ELEMENT STATUS.
///
/// Returns `Ok(None)` to proceed with dispatch, or
/// `Ok(Some(check_condition_response))` to refuse the command. Mirrors
/// the historical fence in thurvtl's `dispatch_scsi`. An
/// unpartitioned topology has its facade return `None` from
/// `partition_drive_ids` and this fence is effectively a no-op.
pub fn check_partition_fence(ctx: &ScsiCtx<'_>) -> Result<Option<ScsiResp>> {
    let Some(part_name) = ctx.session_partition else {
        return Ok(None);
    };
    if ctx.is_changer_lun() {
        return Ok(None);
    }

    let owned = ctx
        .facade
        .partition_drive_ids(part_name)
        .map(|ids| ids.contains(&(ctx.drive_id as u32)))
        .unwrap_or(false);
    if owned {
        return Ok(None);
    }

    tracing::warn!(
        "partition fence: session bound to '{}' tried to address drive {} (LUN {}) not in partition",
        part_name,
        ctx.drive_id,
        ctx.lun,
    );
    // SPC-4 §5.5.2 / SAM-5 §5.5: LUN that the application client has
    // not been granted access to → CHECK CONDITION + ILLEGAL REQUEST
    // + ASC/ASCQ 0x25/0x00 (LOGICAL UNIT NOT SUPPORTED).
    let sense = scsi::sense::SenseDataBuilder::new(
        scsi::sense::SenseKey::IllegalRequest,
        scsi::sense::AdditionalSenseCode {
            asc: 0x25,
            ascq: 0x00,
        },
    )
    .build();
    Ok(Some(ScsiResp::check_condition_with_sense(sense)))
}

/// Drive-LUN MODE SENSE(6) — emits the live mode pages for the drive
/// LUN at `ctx.lun`. thurvtl's wrapper intercepts the LUN-0
/// (changer) path before delegating. Composes the per-page mode-pages
/// helper with the loaded cartridge's partition / compression
/// snapshot and the drive-side saved-page state — same surface the
/// historical thurvtl handler exposed.
pub fn handle_mode_sense_6_drive(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;

    let alloc = pdu_expected_xfer_len(ctx.pdu);
    let page_code = cdb[2] & 0x3F;
    let subpage_code = cdb[3];
    let pc = (cdb[2] >> 6) & 0x03;
    let dbd = (cdb[1] & 0x08) != 0;

    tracing::debug!(
        "MODE SENSE(6): LUN={}, page_code=0x{:02x}, subpage=0x{:02x}, PC={}, DBD={}, alloc={}",
        lun,
        page_code,
        subpage_code,
        pc,
        dbd,
        alloc
    );

    let pc = scsi::mode_pages::PageControl::from(pc);
    let (snapshot, comp_snapshot) = drive_manager
        .with_drive(drive_id, tsih, |cart| {
            Ok((
                scsi::mode_pages::PartitionSnapshot {
                    partition_count: cart.partition_count(),
                },
                scsi::mode_pages::CompressionSnapshot {
                    dce: cart.compression_state().dce,
                },
            ))
        })
        .unwrap_or((
            scsi::mode_pages::PartitionSnapshot { partition_count: 1 },
            scsi::mode_pages::CompressionSnapshot { dce: false },
        ));
    // Saved-mode-page state lives on the *drive* (emulated NVRAM),
    // not on the cartridge — survives cartridge swaps just like real
    // LTO.
    let saved_pages = drive_manager.mode_pages_state(drive_id).unwrap_or_default();
    match scsi::mode_pages::handle_mode_sense_6(
        page_code,
        subpage_code,
        pc,
        dbd,
        snapshot,
        comp_snapshot,
        &saved_pages,
    ) {
        Ok(data) => {
            tracing::debug!(
                "MODE SENSE(6) response for Tape Drive: {} bytes",
                data.len()
            );
            Ok(ScsiResp {
                status: ScsiStatus::Good,
                data_out: limit_len(data, alloc),
                sense: None,
            })
        }
        Err(e) => {
            tracing::warn!("MODE SENSE(6) error: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

/// Drive-LUN MODE SENSE(10). Same surface as
/// [`handle_mode_sense_6_drive`] with the 10-byte CDB form (LLBAA bit,
/// 16-bit allocation length).
pub fn handle_mode_sense_10_drive(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;

    let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as u32;
    let page_code = cdb[2] & 0x3F;
    let subpage_code = cdb[3];
    let pc = (cdb[2] >> 6) & 0x03;
    let dbd = (cdb[1] & 0x08) != 0;
    let llbaa = (cdb[1] & 0x10) != 0;

    tracing::debug!(
        "MODE SENSE(10): LUN={}, page_code=0x{:02x}, subpage=0x{:02x}, PC={}, DBD={}, LLBAA={}, alloc={}",
        lun,
        page_code,
        subpage_code,
        pc,
        dbd,
        llbaa,
        alloc
    );

    let pc = scsi::mode_pages::PageControl::from(pc);
    let (snapshot, comp_snapshot) = drive_manager
        .with_drive(drive_id, tsih, |cart| {
            Ok((
                scsi::mode_pages::PartitionSnapshot {
                    partition_count: cart.partition_count(),
                },
                scsi::mode_pages::CompressionSnapshot {
                    dce: cart.compression_state().dce,
                },
            ))
        })
        .unwrap_or((
            scsi::mode_pages::PartitionSnapshot { partition_count: 1 },
            scsi::mode_pages::CompressionSnapshot { dce: false },
        ));
    let saved_pages = drive_manager.mode_pages_state(drive_id).unwrap_or_default();
    match scsi::mode_pages::handle_mode_sense_10(
        page_code,
        subpage_code,
        pc,
        dbd,
        llbaa,
        snapshot,
        comp_snapshot,
        &saved_pages,
    ) {
        Ok(data) => {
            tracing::debug!("MODE SENSE(10) response: {} bytes", data.len());
            Ok(ScsiResp {
                status: ScsiStatus::Good,
                data_out: limit_len(data, alloc),
                sense: None,
            })
        }
        Err(e) => {
            tracing::warn!("MODE SENSE(10) error: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

/// Drive-LUN MODE SELECT(6). Parses the parameter list, applies any
/// per-page side effects (partition-layout staging, DCE bit, raw
/// round-trip into the saved-page state) and emits an audit entry on
/// the DCE flip. thurvtl's wrapper intercepts the LUN-0 (changer)
/// no-op before delegating.
pub fn handle_mode_select_6_drive(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let lun = ctx.lun;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;
    let audit_log = ctx.audit_log;
    let audit_ratelimiter = ctx.audit_ratelimiter;
    let cdb = ctx.cdb;

    let sp = (cdb[1] & 0x01) != 0;
    tracing::debug!(
        "MODE SELECT(6): LUN={}, SP={}, {} bytes of data",
        lun,
        sp,
        ctx.pdu.data.len()
    );
    match scsi::mode_pages::handle_mode_select_6(sp, &ctx.pdu.data) {
        Ok(outcome) => apply_mode_select_outcome(
            ctx,
            outcome,
            drive_id,
            tsih,
            drive_manager,
            audit_log,
            audit_ratelimiter,
            "MODE_SELECT_6",
        ),
        Err(e) => {
            tracing::warn!("MODE SELECT(6) error: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

/// Drive-LUN MODE SELECT(10). Same surface as
/// [`handle_mode_select_6_drive`] with the 10-byte CDB form.
pub fn handle_mode_select_10_drive(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let lun = ctx.lun;
    let drive_id = ctx.drive_id;
    let tsih = ctx.tsih;
    let drive_manager = ctx.drive_manager;
    let audit_log = ctx.audit_log;
    let audit_ratelimiter = ctx.audit_ratelimiter;
    let cdb = ctx.cdb;

    let sp = (cdb[1] & 0x01) != 0;
    tracing::debug!(
        "MODE SELECT(10): LUN={}, SP={}, {} bytes of data",
        lun,
        sp,
        ctx.pdu.data.len()
    );
    match scsi::mode_pages::handle_mode_select_10(sp, &ctx.pdu.data) {
        Ok(outcome) => apply_mode_select_outcome(
            ctx,
            outcome,
            drive_id,
            tsih,
            drive_manager,
            audit_log,
            audit_ratelimiter,
            "MODE_SELECT_10",
        ),
        Err(e) => {
            tracing::warn!("MODE SELECT(10) error: {}", e);
            Ok(ScsiResp::check_condition())
        }
    }
}

/// Apply the per-page side effects parsed out of a MODE SELECT(6/10)
/// parameter list. Order matters:
///   1. Page 0x11 partition-layout staging (rejection here aborts).
///   2. Page 0x0F DCE bit (audited).
///   3. Round-trip raw bodies for every page in the parameter list,
///      with SP=1 persistence into the manifest.
#[allow(clippy::too_many_arguments)]
fn apply_mode_select_outcome(
    ctx: &mut ScsiCtx<'_>,
    outcome: scsi::mode_pages::ModeSelectOutcome,
    drive_id: usize,
    tsih: u16,
    drive_manager: &std::sync::Arc<drive_manager::DriveManager>,
    audit_log: &Option<core_mediachanger::AuditChannel>,
    audit_ratelimiter: &core_mediachanger::AuditRateLimiter,
    cdb_label: &str,
) -> Result<ScsiResp> {
    let lun = ctx.lun;
    if let Some(layout) = outcome.pending_layout {
        tracing::info!(
            "{} page 0x11: staging partition layout (additional={}, idp={}, sizes={:?})",
            cdb_label,
            layout.additional_partitions,
            layout.idp,
            layout.partition_sizes
        );
        if let Err(e) = drive_manager.with_drive(drive_id, tsih, |cart| {
            cart.set_pending_partition_layout(layout)
        }) {
            tracing::warn!("{} page 0x11 rejected: {}", cdb_label, e);
            return Ok(ScsiResp::check_condition());
        }
    }
    if let Some(dce) = outcome.compression_dce {
        tracing::info!("{} page 0x0F: drive {} DCE -> {}", cdb_label, drive_id, dce);
        let res = drive_manager.with_drive(drive_id, tsih, |cart| {
            let mut state = cart.compression_state();
            state.dce = dce;
            cart.set_compression_state(state);
            Ok(())
        });
        audit_append(
            audit_log,
            audit_ratelimiter,
            "iscsi.drive_compression",
            ctx.audit_actor(),
            serde_json::json!({
                "drive": drive_id,
                "lun": lun,
                "dce": dce,
                "cdb": cdb_label,
            }),
            match &res {
                Ok(()) => AuditResult::Ok,
                Err(e) => AuditResult::Error(e.to_string()),
            },
        );
    }
    if !outcome.saved_pages.is_empty()
        && let Err(e) = drive_manager.apply_mode_select_pages(
            drive_id,
            &outcome.saved_pages,
            outcome.save_pages,
        )
    {
        tracing::warn!(
            "{}: failed to update drive mode-page state: {}",
            cdb_label,
            e
        );
        return Ok(ScsiResp::check_condition_for(&e));
    }
    Ok(ScsiResp::good())
}

/// LOG SELECT — backup software occasionally uses this to clear log
/// counters (PCR=1) or to select a saved/default page-control value
/// for subsequent LOG SENSE calls. Neither tape product keeps
/// persistent log counters, so this accepts the request unconditionally
/// and treats it as a no-op. The PCR bit and parameter list are
/// ignored.
pub fn handle_log_select(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let lun = ctx.lun;
    let pcr = (cdb[1] & 0x02) != 0;
    let pc = (cdb[2] >> 6) & 0x03;
    tracing::debug!(
        "LOG SELECT: LUN={}, PCR={}, PC={}, parameter_list={} bytes",
        lun,
        pcr,
        pc,
        ctx.pdu.data.len()
    );
    Ok(ScsiResp::good())
}

/// RECEIVE DIAGNOSTIC RESULTS (SPC-4 §6.21). Walks
/// `ctx.diagnostic_store` to emit either the Supported Diagnostic
/// Pages list (page 0x00) or the Self-Test Results page (page 0x10).
/// PCV=0 is treated as a request for page 0x00 since that's what
/// initiators poll first.
pub fn handle_receive_diagnostic_results(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let pcv = (cdb[1] & 0x01) != 0;
    let page_code = cdb[2];
    let alloc = u16::from_be_bytes([cdb[3], cdb[4]]) as u32;
    let effective = if pcv { page_code } else { 0x00 };

    tracing::debug!(
        "RECEIVE DIAGNOSTIC RESULTS: LUN={} PCV={} page=0x{:02x} alloc={}",
        ctx.lun,
        pcv,
        page_code,
        alloc
    );

    let data = match effective {
        0x00 => crate::diagnostics::build_supported_diagnostic_pages(),
        0x10 => crate::diagnostics::build_self_test_results_page(ctx.diagnostic_store, ctx.lun),
        _ => {
            // SPC-4 §6.21: unsupported page → CHECK CONDITION /
            // ILLEGAL REQUEST / INVALID FIELD IN CDB.
            let sense = scsi::sense::SenseDataBuilder::new(
                scsi::sense::SenseKey::IllegalRequest,
                scsi::sense::ASC_INVALID_FIELD_IN_CDB,
            )
            .build();
            return Ok(ScsiResp::check_condition_with_sense(sense));
        }
    };

    Ok(ScsiResp {
        status: ScsiStatus::Good,
        data_out: limit_len(data, alloc),
        sense: None,
    })
}

/// SEND DIAGNOSTIC. SELFTEST=1 (CDB byte 1 bit 2) is the only trigger
/// we act on; everything else (default no-op probe, parameter-list
/// tests, foreground/background extended self-test codes) returns
/// GOOD without recording. thurvtl wires its `run_and_record`
/// pre-hook from its iSCSI dispatch path and runs before this sync
/// handler — by the time we get here the freshest entry is already in
/// `ctx.diagnostic_store`. The handler only has to surface GOOD vs
/// CHECK CONDITION.
pub fn handle_send_diagnostic(ctx: &mut ScsiCtx<'_>) -> Result<ScsiResp> {
    let cdb = ctx.cdb;
    let selftest = (cdb[1] & 0x04) != 0;
    let self_test_code = (cdb[1] >> 5) & 0x07;

    tracing::debug!(
        "SEND DIAGNOSTIC: LUN={} SELFTEST={} self_test_code={} parameter_list={} bytes",
        ctx.lun,
        selftest,
        self_test_code,
        ctx.pdu.data.len()
    );

    if !selftest {
        return Ok(ScsiResp::good());
    }

    match ctx.diagnostic_store.last(ctx.lun) {
        Some(entry) if entry.passed => Ok(ScsiResp::good()),
        Some(entry) => {
            let sense = scsi::sense::SenseDataBuilder::new(
                match entry.sense_key {
                    0x04 => scsi::sense::SenseKey::HardwareError,
                    0x05 => scsi::sense::SenseKey::IllegalRequest,
                    _ => scsi::sense::SenseKey::HardwareError,
                },
                scsi::sense::AdditionalSenseCode {
                    asc: entry.asc,
                    ascq: entry.ascq,
                },
            )
            .build();
            Ok(ScsiResp::check_condition_with_sense(sense))
        }
        // Pre-hook should always populate when SELFTEST=1 on
        // thurvtl; defensive GOOD here in case the pre-hook is
        // somehow bypassed.
        None => Ok(ScsiResp::good()),
    }
}
