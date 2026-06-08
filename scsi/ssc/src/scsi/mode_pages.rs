// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// MODE SENSE/MODE SELECT implementation for SSC-2 tape drives
// Reference: SCSI Stream Commands (SSC-2) specification
//
// Complete SCSI mode page implementation per spec.

#![allow(dead_code)]

use core_mediachanger::{DrivePageStore, PendingPartitionLayout};
use scsi_spc::mode::{
    MODE_PARAM_HEADER_6_LEN, MODE_PARAM_HEADER_10_LEN, parse_mode_param_header_6,
    parse_mode_param_header_10, patch_mode_data_length_6, patch_mode_data_length_10,
    write_mode_param_header_6, write_mode_param_header_10,
};
use tracing::{info, warn};

/// Mode page codes for tape drives (SSC-2)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ModePageCode {
    ReadWriteErrorRecovery = 0x01,
    DisconnectReconnect = 0x02,
    ControlMode = 0x0A,
    DataCompression = 0x0F,
    DeviceConfiguration = 0x10,
    MediumPartition = 0x11,
    PowerCondition = 0x1A,
    InformationalExceptionsControl = 0x1C,
    AllPages = 0x3F,
}

/// Page Control field values (PC) in MODE SENSE
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageControl {
    Current = 0x00,
    Changeable = 0x01,
    Default = 0x02,
    Saved = 0x03,
}

impl From<u8> for PageControl {
    fn from(val: u8) -> Self {
        match val {
            0x00 => PageControl::Current,
            0x01 => PageControl::Changeable,
            0x02 => PageControl::Default,
            0x03 => PageControl::Saved,
            _ => PageControl::Current,
        }
    }
}

/// Snapshot of the active partition layout for the MODE SENSE 0x11 builder.
/// Sourced from the cartridge so the host sees the truth of the medium.
#[derive(Debug, Clone, Copy, Default)]
pub struct PartitionSnapshot {
    pub partition_count: u8,
}

/// Snapshot of drive-level compression state for the MODE SENSE 0x0F
/// builder. We always advertise DCC=1 (compression capable). DCE follows
/// the runtime drive state so the host sees the truth — toggling DCE via
/// MODE SELECT is what flips this. Default (no cartridge loaded) is
/// "capable, currently disabled".
#[derive(Debug, Clone, Copy, Default)]
pub struct CompressionSnapshot {
    /// Current Data Compression Enable bit on the drive.
    pub dce: bool,
}

/// Handle MODE SENSE(6) command (0x1A)
/// Returns mode parameter header + requested mode page(s)
pub fn handle_mode_sense_6(
    page_code: u8,
    subpage_code: u8,
    pc: PageControl,
    dbd: bool, // Disable Block Descriptors
    snapshot: PartitionSnapshot,
    compression: CompressionSnapshot,
    saved: &DrivePageStore,
) -> Result<Vec<u8>, String> {
    info!(
        "MODE SENSE(6): page_code=0x{:02x}, subpage=0x{:02x}, PC={:?}, DBD={}",
        page_code, subpage_code, pc, dbd
    );

    let mut response = Vec::new();

    // MODE PARAMETER HEADER (4 bytes for MODE SENSE(6)).
    // medium_type=0 (default), device_specific=0 (WP=0).
    write_mode_param_header_6(&mut response, 0x00, 0x00, if dbd { 0 } else { 8 });

    // Add block descriptor if not disabled (8 bytes)
    if !dbd {
        // Block descriptor for tape (SSC-4 §8.3.4). thurvtl stores tape data
        // as variable-length blocks (one block per WRITE PDU), so we advertise
        // block length 0 — that tells the Linux st driver to operate in
        // variable-block mode (each read()/write() syscall = one tape block,
        // no kernel-side splitting into 512-byte fixed blocks).
        response.push(0x00); // Density code (0 = default)
        response.extend_from_slice(&[0x00, 0x00, 0x00]); // Number of blocks (0 = all available)
        response.push(0x00); // Reserved
        response.extend_from_slice(&[0x00, 0x00, 0x00]); // Block length 0 = variable-block mode
    }

    append_tape_pages(
        &mut response,
        page_code,
        subpage_code,
        pc,
        snapshot,
        compression,
        saved,
    )?;

    // Patch MODE DATA LENGTH (byte 0; total length - 1).
    patch_mode_data_length_6(&mut response, 0);

    info!("MODE SENSE(6) response: {} bytes", response.len());
    Ok(response)
}

/// Handle MODE SENSE(10) command (0x5A)
/// Similar to MODE SENSE(6) but with 10-byte CDB and longer header
pub fn handle_mode_sense_10(
    page_code: u8,
    subpage_code: u8,
    pc: PageControl,
    dbd: bool,
    llbaa: bool, // Long LBA Accepted
    snapshot: PartitionSnapshot,
    compression: CompressionSnapshot,
    saved: &DrivePageStore,
) -> Result<Vec<u8>, String> {
    info!(
        "MODE SENSE(10): page_code=0x{:02x}, subpage=0x{:02x}, PC={:?}, DBD={}, LLBAA={}",
        page_code, subpage_code, pc, dbd, llbaa
    );

    let mut response = Vec::new();

    // MODE PARAMETER HEADER (8 bytes for MODE SENSE(10)).
    // medium_type=0 (default), device_specific=0. Declared
    // block-descriptor length must match what we actually emit below:
    // 16 for the long-LBA descriptor, 8 for the short one, 0 when DBD
    // suppresses it.
    let block_descriptor_length: u16 = if dbd {
        0
    } else if llbaa {
        16
    } else {
        8
    };
    write_mode_param_header_10(&mut response, 0x00, 0x00, llbaa, block_descriptor_length);

    // Add block descriptor if not disabled
    if !dbd {
        if llbaa {
            // Long LBA block descriptor (SPC-3 §7.5.6, 16 bytes):
            // 64-bit NUMBER OF LOGICAL BLOCKS (0 = all available) +
            // 4 reserved + 32-bit BLOCK LENGTH (0 = variable-block
            // mode). Same variable-block semantics as the short
            // descriptor below — just the long format the host
            // requested via LLBAA=1.
            response.extend_from_slice(&[0x00; 8]); // number of logical blocks = 0
            response.extend_from_slice(&[0x00; 4]); // reserved
            response.extend_from_slice(&[0x00; 4]); // block length 0 = variable
        } else {
            // Short block descriptor (8 bytes). Same rationale as MODE SENSE(6):
            // advertise block length 0 to keep the kernel in variable-block mode.
            response.push(0x00); // Density code
            response.extend_from_slice(&[0x00, 0x00, 0x00]); // Number of blocks
            response.push(0x00); // Reserved
            response.extend_from_slice(&[0x00, 0x00, 0x00]); // Block length 0 = variable
        }
    }

    append_tape_pages(
        &mut response,
        page_code,
        subpage_code,
        pc,
        snapshot,
        compression,
        saved,
    )?;

    // Patch MODE DATA LENGTH (bytes 0..2; total length - 2).
    patch_mode_data_length_10(&mut response, 0);

    info!("MODE SENSE(10) response: {} bytes", response.len());
    Ok(response)
}

/// Shared mode-page builder for MODE SENSE(6) and (10). Routes
/// `(page_code, subpage_code)` per SPC-3 / SSC-4:
///   - subpage_code = 0x00: only SPF=0 pages (current model: 0x01,
///     0x0F, 0x10, 0x11, 0x1C). page_code 0x3F includes every SPF=0 page.
///   - subpage_code = 0xFF: include every SPF=1 subpage too. With
///     page_code 0x3F that's "every page and every subpage" — host's
///     full inventory request.
///   - subpage_code = 0x01 + page_code 0x10: Device Configuration
///     Extension (LTO-7+ Append-only mode + LTO-8+ Encrypt-only
///     mode). thurvtl doesn't model either feature; page emits with
///     all default fields zero so backup software that polls it for
///     capability detection sees "standard mode, no append-only, no
///     encrypt-only".
fn append_tape_pages(
    response: &mut Vec<u8>,
    page_code: u8,
    subpage_code: u8,
    pc: PageControl,
    snapshot: PartitionSnapshot,
    compression: CompressionSnapshot,
    saved: &DrivePageStore,
) -> Result<(), String> {
    let all_pages = page_code == 0x3F;
    let include_subpages = subpage_code == 0xFF;

    // SPF=0 pages — only when subpage_code is 0x00 or 0xFF.
    if subpage_code == 0x00 || subpage_code == 0xFF {
        match page_code {
            0x00 => {
                // Supported Pages list (one byte per supported page code).
                response.push(0x00);
                response.push(0x07);
                response.push(0x01);
                response.push(0x02);
                response.push(0x0F);
                response.push(0x10);
                response.push(0x11);
                response.push(0x1A);
                response.push(0x1C);
            }
            0x01 => add_read_write_error_recovery(response, pc, saved),
            0x02 => add_disconnect_reconnect(response, pc, saved),
            0x0F => add_data_compression(response, pc, compression, saved),
            0x10 => add_device_configuration(response, pc, saved),
            0x11 => add_medium_partition(response, pc, snapshot),
            0x1A => add_power_condition(response, pc, saved),
            0x1C => add_informational_exceptions_control(response, pc, saved),
            0x3F => {
                add_read_write_error_recovery(response, pc, saved);
                add_disconnect_reconnect(response, pc, saved);
                add_data_compression(response, pc, compression, saved);
                add_device_configuration(response, pc, saved);
                add_medium_partition(response, pc, snapshot);
                add_power_condition(response, pc, saved);
                add_informational_exceptions_control(response, pc, saved);
            }
            _ if !all_pages => {
                warn!("MODE SENSE: Unsupported page code 0x{:02x}", page_code);
                return Err(format!("Unsupported mode page: 0x{:02x}", page_code));
            }
            _ => {}
        }
    }

    // SPF=1 subpages.
    if (page_code == 0x10 && subpage_code == 0x01) || (all_pages && include_subpages) {
        add_device_configuration_extension(response, pc, saved);
    }
    if (page_code == 0x0A && subpage_code == 0xF0) || (all_pages && include_subpages) {
        add_control_data_protection(response, pc, saved);
    }

    Ok(())
}

/// Apply the PS (Parameters Saveable) bit to a page header byte. We
/// always advertise PS=1 — every mode page on this drive is saveable
/// via MODE SELECT with SP=1. SPF=0 pages have a 1-byte page-code
/// field where bit 7 is PS and bit 6 is SPF (SubPage Format) — both
/// live in the same byte. SPF=1 subpages put PS in the same bit.
fn ps_set(page_code: u8) -> u8 {
    page_code | 0x80
}

/// Result of parsing a MODE SELECT parameter list.
///
/// Two layers of state come out of the parser:
///   1. **Behavior-driving fields** (`pending_layout` for page 0x11,
///      `compression_dce` for page 0x0F's DCE bit) — applied to the
///      cartridge via dedicated setters by the caller.
///   2. **Round-trip raw bodies** (`saved_pages`) — every page that
///      appeared in the parameter list, byte-for-byte. Applied via
///      `Cartridge::apply_mode_select_pages` so a subsequent MODE
///      SENSE replays the host's bytes verbatim under
///      PC=Current / PC=Saved (SPC-4 round-trip requirement).
///
/// `save_pages` carries the SP bit out of the MODE SELECT CDB so the
/// caller knows whether to mirror the bodies into the manifest for
/// persistence across mount cycles.
#[derive(Debug, Default)]
pub struct ModeSelectOutcome {
    pub pending_layout: Option<PendingPartitionLayout>,
    pub compression_dce: Option<bool>,
    /// `(page_code, subpage_code, body)` for every page in the
    /// parameter list. Bodies exclude the 2- or 4-byte page header.
    pub saved_pages: Vec<(u8, u8, Vec<u8>)>,
    /// SP (Save Pages) bit from MODE SELECT CDB byte 1 bit 0. When
    /// true the caller should persist `saved_pages` into the manifest.
    pub save_pages: bool,
}

/// Handle MODE SELECT(6) command (0x15). `sp` is the SP (Save Pages)
/// bit from CDB byte 1 bit 0 — propagates onto `ModeSelectOutcome` so
/// the caller knows whether to persist the bodies after applying.
/// Parameter list parsing is real for page 0x11 (Medium Partition) and
/// page 0x0F (Data Compression DCE bit); every other page is captured
/// as a raw round-trip body for the host to read back via MODE SENSE.
pub fn handle_mode_select_6(sp: bool, data: &[u8]) -> Result<ModeSelectOutcome, String> {
    info!(
        "MODE SELECT(6): SP={}, {} bytes of parameter data",
        sp,
        data.len()
    );

    if data.is_empty() {
        return Ok(ModeSelectOutcome {
            save_pages: sp,
            ..Default::default()
        });
    }

    // Parse MODE PARAMETER HEADER (4 bytes).
    let Some(header) = parse_mode_param_header_6(data) else {
        warn!("MODE SELECT(6): Invalid parameter list (too short)");
        return Err("Invalid parameter list".to_string());
    };

    let block_desc_len = header.block_descriptor_length as usize;
    let page_start = MODE_PARAM_HEADER_6_LEN + block_desc_len;

    if data.len() < page_start {
        warn!("MODE SELECT(6): Invalid block descriptor length");
        return Err("Invalid block descriptor length".to_string());
    }

    let mut outcome = parse_mode_pages(&data[page_start..])?;
    outcome.save_pages = sp;
    Ok(outcome)
}

/// Handle MODE SELECT(10) command (0x55). `sp` carries the Save Pages
/// bit from CDB byte 1 bit 0.
pub fn handle_mode_select_10(sp: bool, data: &[u8]) -> Result<ModeSelectOutcome, String> {
    info!(
        "MODE SELECT(10): SP={}, {} bytes of parameter data",
        sp,
        data.len()
    );

    if data.is_empty() {
        return Ok(ModeSelectOutcome {
            save_pages: sp,
            ..Default::default()
        });
    }

    // Parse MODE PARAMETER HEADER (8 bytes).
    let Some(header) = parse_mode_param_header_10(data) else {
        warn!("MODE SELECT(10): Invalid parameter list (too short)");
        return Err("Invalid parameter list".to_string());
    };

    let block_desc_len = header.block_descriptor_length as usize;
    let page_start = MODE_PARAM_HEADER_10_LEN + block_desc_len;

    if data.len() < page_start {
        warn!("MODE SELECT(10): Invalid block descriptor length");
        return Err("Invalid block descriptor length".to_string());
    }

    let mut outcome = parse_mode_pages(&data[page_start..])?;
    outcome.save_pages = sp;
    Ok(outcome)
}

/// Walk a sequence of mode pages and pull out everything the caller
/// needs. SPC-4 page header layout depends on the SPF (SubPage Format)
/// bit in byte 0:
///
///   SPF=0  2-byte header: [page_code, page_length], body length = page_length.
///   SPF=1  4-byte header: [page_code|0x40, subpage_code, page_length(BE16)],
///          body length = page_length.
///
/// For SPF=1 we still mask the page_code with 0x3F. Subpage code 0
/// always means "SPF=0 page" — never appears in the SPF=1 form.
///
/// Per-page side effects:
///   (0x11, 0x00) → pending partition layout (LTFS FORMAT MEDIUM 0x01).
///   (0x0F, 0x00) → DCE bit toggle on the drive's compression state.
///
/// Every parsed page is also recorded in `saved_pages` so MODE SENSE
/// can replay the bytes verbatim — that's the SPC-4 round-trip
/// requirement.
fn parse_mode_pages(mut buf: &[u8]) -> Result<ModeSelectOutcome, String> {
    let mut outcome = ModeSelectOutcome::default();
    while buf.len() >= 2 {
        let spf = (buf[0] & 0x40) != 0;
        let page_code = buf[0] & 0x3F;
        let (subpage_code, page_len, header_len) = if spf {
            if buf.len() < 4 {
                warn!(
                    "MODE SELECT: truncated SPF=1 header for page 0x{:02x}",
                    page_code
                );
                return Err("Truncated SPF=1 page header".to_string());
            }
            let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            (buf[1], len, 4usize)
        } else {
            (0u8, buf[1] as usize, 2usize)
        };
        if buf.len() < header_len + page_len {
            warn!(
                "MODE SELECT: truncated page 0x{:02x}/0x{:02x}, declared {} bytes but only {} remain",
                page_code,
                subpage_code,
                page_len,
                buf.len().saturating_sub(header_len)
            );
            return Err("Truncated mode page".to_string());
        }
        let page_data = &buf[header_len..header_len + page_len];

        // Side-effecting fields.
        match (page_code, subpage_code) {
            (0x11, 0x00) => {
                outcome.pending_layout = Some(parse_medium_partition_page(page_data)?);
            }
            (0x0F, 0x00) => {
                if page_data.is_empty() {
                    return Err("MODE SELECT page 0x0F: empty body".to_string());
                }
                outcome.compression_dce = Some((page_data[0] & 0x80) != 0);
            }
            _ => {}
        }

        // Round-trip raw body for MODE SENSE replay.
        outcome
            .saved_pages
            .push((page_code, subpage_code, page_data.to_vec()));

        buf = &buf[header_len + page_len..];
    }
    Ok(outcome)
}

// ============================================================================
// Mode Page Builders
// ============================================================================

/// Page 0x01: Read-Write Error Recovery (12 bytes total = 2-byte
/// header + 10-byte body). Default body is all-zero (no auto
/// reallocation, no retries — virtual media has nothing to recover
/// from). Round-trippable: every byte the host writes via MODE SELECT
/// is replayed verbatim under PC=Current / PC=Saved. Changeable mask
/// advertises every byte tunable.
fn add_read_write_error_recovery(response: &mut Vec<u8>, pc: PageControl, saved: &DrivePageStore) {
    const DEFAULT: [u8; 10] = [0u8; 10];
    response.push(ps_set(0x01));
    response.push(0x0A);
    let body = match pc {
        PageControl::Current | PageControl::Saved => saved
            .get(0x01, 0x00)
            .filter(|b| b.len() == DEFAULT.len())
            .map(|b| b.to_vec())
            .unwrap_or_else(|| DEFAULT.to_vec()),
        PageControl::Default => DEFAULT.to_vec(),
        PageControl::Changeable => vec![0xFFu8; DEFAULT.len()],
    };
    response.extend_from_slice(&body);
}

/// Page 0x02: Disconnect-Reconnect (SPC-3 §7.4.5). 16 bytes total
/// (2-byte page header + 14-byte body, page-length=0x0E).
///
/// Originally a parallel-SCSI knob (bus inactivity / disconnect time /
/// burst sizes). Per SPC-3 Annex G the legacy fields are ignored on
/// transports that don't support disconnect/reconnect — which includes
/// iSCSI, the only transport thurvtl ships. Backup software still
/// polls the page during drive-capability sweeps; absence triggers
/// warnings and occasionally a fallback path.
///
/// Body layout:
///   byte 0    Buffer full ratio (legacy, 0)
///   byte 1    Buffer empty ratio (legacy, 0)
///   2..=3     Bus inactivity limit (100 us, 0)
///   4..=5     Disconnect time limit (100 us, 0)
///   6..=7     Connect time limit (100 us, 0)
///   8..=9     Maximum burst size (512 B units, 0 = no limit)
///   byte 10   bit 7 EMDP | bits 6..4 Fair Arbitration | bit 3 DIMM |
///             bits 2..0 DTDC
///   byte 11   Reserved
///   12..=13   First burst size (512 B units, 0 = no limit)
///
/// All zero is the spec'd "transport doesn't model disconnect" answer.
/// PC=Changeable reports zero — no fields are host-tunable.
fn add_disconnect_reconnect(response: &mut Vec<u8>, pc: PageControl, saved: &DrivePageStore) {
    const DEFAULT: [u8; 14] = [0u8; 14];
    response.push(ps_set(0x02));
    response.push(0x0E);
    let body = match pc {
        PageControl::Current | PageControl::Saved => saved
            .get(0x02, 0x00)
            .filter(|b| b.len() == DEFAULT.len())
            .map(|b| b.to_vec())
            .unwrap_or_else(|| DEFAULT.to_vec()),
        PageControl::Default => DEFAULT.to_vec(),
        // Disconnect-Reconnect knobs are all parallel-SCSI legacy and
        // ignored on iSCSI per SPC-3 Annex G — leave Changeable=0 so
        // we don't claim host-tunable bytes that have no effect.
        PageControl::Changeable => DEFAULT.to_vec(),
    };
    response.extend_from_slice(&body);
}

/// Page 0x0F: Data Compression (16 bytes). SSC-4 §8.3.4.3.
///
/// We always advertise DCC=1 (Data Compression Capable) — thurvtl is
/// a real compression-capable drive (zstd at the block level when DCE
/// is on). DCE/DDE follow the runtime drive state so MODE SELECT
/// toggles the host sees back via MODE SENSE are truthful. Algorithm
/// codes are reported as 0 ("vendor-specific / default") because zstd
/// has no SCSI-registered algorithm code; real LTO drives report 0x40
/// for LTO-DC, but we are not LZ77-encoding LTO-DC bytes on the wire,
/// so the honest answer is 0.
///
/// Byte layout (after the 2-byte page header):
///   byte 0  bit 7  DCE (Data Compression Enable)
///   byte 0  bit 6  DCC (Data Compression Capable) — always 1 for us
///   byte 1  bit 7  DDE (Data Decompression Enable)
///   byte 1  bits 6:5  RED (Report Exception on Decompression)
///   bytes 2-5  Compression algorithm (32-bit big-endian)
///   bytes 6-9  Decompression algorithm (32-bit big-endian)
///   bytes 10-13  Reserved
fn add_data_compression(
    response: &mut Vec<u8>,
    pc: PageControl,
    comp: CompressionSnapshot,
    saved: &DrivePageStore,
) {
    response.push(ps_set(0x0F));
    response.push(0x0E);

    match pc {
        PageControl::Current | PageControl::Saved => {
            // Start from the saved body if present, else from defaults.
            // Then overwrite byte 0 to reflect the *runtime* DCE bit —
            // that's the truth of the drive, regardless of what the
            // host last MODE SELECTed (the daemon config or a pre-host
            // reset may have changed it underneath).
            let mut body: Vec<u8> = saved
                .get(0x0F, 0x00)
                .filter(|b| b.len() == 14)
                .map(|b| b.to_vec())
                .unwrap_or_else(|| {
                    vec![
                        0x40, 0x80, // DCE=0|DCC=1, DDE=1|RED=0
                        0x00, 0x00, 0x00, 0x00, // Compression algorithm
                        0x00, 0x00, 0x00, 0x00, // Decompression algorithm
                        0x00, 0x00, 0x00, 0x00, // Reserved
                    ]
                });
            // Force runtime DCE | DCC=1; host can't undo DCC.
            body[0] = if comp.dce { 0xC0 } else { 0x40 };
            response.extend_from_slice(&body);
        }
        PageControl::Default => {
            response.extend_from_slice(&[
                0x40, // DCE=0, DCC=1
                0x80, // DDE=1, RED=0
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ]);
        }
        PageControl::Changeable => {
            // Host-tunable: DCE (byte 0 bit 7), DDE/RED (byte 1 bits 7..5),
            // compression + decompression algorithm codes (bytes 2..9).
            response.extend_from_slice(&[
                0x80, // DCE changeable; DCC fixed (we are always capable)
                0xE0, // DDE | RED
                0xFF, 0xFF, 0xFF, 0xFF, // compression algorithm
                0xFF, 0xFF, 0xFF, 0xFF, // decompression algorithm
                0x00, 0x00, 0x00, 0x00, // reserved — not changeable
            ]);
        }
    }
}

/// Page 0x10: Device Configuration (SSC-4 §8.3.4.5). 16 bytes total =
/// 2-byte header + 14-byte body. Default emits DBR=1 (Data Buffer
/// Recovery enabled — recover from buffer underrun by automatic
/// retry); every other byte zero. Round-trippable except byte 1
/// (Active Partition) — that one is owned by the cartridge and driven
/// by LOCATE PARTITION, not by MODE SELECT.
fn add_device_configuration(response: &mut Vec<u8>, pc: PageControl, saved: &DrivePageStore) {
    const DEFAULT: [u8; 14] = [
        0x00, // Active format (0 = default)
        0x00, // Active partition (overwritten with cartridge truth below)
        0x00, // Write buffer full ratio
        0x00, // Read buffer empty ratio
        0x00, 0x00, // Write delay time
        0x80, // DBR=1, BIS=0, RSMK=0, AVC=0, SOCF=0, RBO=0, REW=0
        0x00, // Gap size
        0x00, // EOD defined / EEG / SEW
        0x00, 0x00, 0x00, // Buffer size at early warning
        0x00, // Select data compression algorithm (deprecated)
        0x00, // WTRE / OIR / ASOCWP / PERSWP / PRMWP
    ];
    response.push(ps_set(0x10));
    response.push(0x0E);
    let body = match pc {
        PageControl::Current | PageControl::Saved => saved
            .get(0x10, 0x00)
            .filter(|b| b.len() == DEFAULT.len())
            .map(|b| b.to_vec())
            .unwrap_or_else(|| DEFAULT.to_vec()),
        PageControl::Default => DEFAULT.to_vec(),
        // Active Partition (byte 1) is not host-tunable via MODE SELECT
        // here — LOCATE PARTITION owns it. Everything else is.
        PageControl::Changeable => {
            let mut mask = vec![0xFFu8; DEFAULT.len()];
            mask[1] = 0x00;
            mask
        }
    };
    response.extend_from_slice(&body);
}

/// Page 0x10 Subpage 0x01: Device Configuration Extension (SSC-5
/// §8.3.4.5). 32 bytes total = 4-byte SPF=1 header + 28-byte body.
///
/// Carries:
///   byte 4 bits 7..4   WRITE MODE (LTO-7+; 0 = default / random,
///                      1 = append-only, 2 = OEM-defined). thurvtl
///                      does not model append-only — always emits 0.
///   byte 6 bits 7..6   WRE (LTO-8+ Encrypt-only mode). thurvtl
///                      does not model encrypt-only — always emits 0.
///   byte 8..9          PEWS (Programmable Early Warning Size in MiB).
///                      thurvtl has no end-of-medium model — emits 0.
///   other bytes        Reserved / advisory; emitted as zero.
///
/// Backup software (NetBackup, Veeam, Bareos) polls this page during
/// drive capability detection. Returning a well-formed all-zero body
/// signals "drive is in standard mode" — no append-only, no
/// encrypt-only, no programmable early warning. Real LTO drives that
/// model these features would set the relevant bits; we don't, so a
/// host that issued MODE SELECT to enable append-only would be told
/// nothing changed (Changeable PC reports zero changeable mask).
fn add_device_configuration_extension(
    response: &mut Vec<u8>,
    pc: PageControl,
    saved: &DrivePageStore,
) {
    const BODY_LEN: usize = 28;
    let mut p = vec![0u8; 4 + BODY_LEN];
    // PS=1 | SPF=1 | page code 0x10
    p[0] = 0x80 | 0x40 | 0x10;
    p[1] = 0x01; // subpage code
    p[2] = 0x00;
    p[3] = BODY_LEN as u8; // page length = 28

    let body = match pc {
        PageControl::Current | PageControl::Saved => saved
            .get(0x10, 0x01)
            .filter(|b| b.len() == BODY_LEN)
            .map(|b| b.to_vec())
            .unwrap_or_else(|| vec![0u8; BODY_LEN]),
        PageControl::Default => vec![0u8; BODY_LEN],
        // Append-only / Encrypt-only / PEWS bytes are host-tunable per
        // SSC-5 even though we don't enforce the modes (round-trip
        // only). Bytes the host tweaks survive in the saved body so
        // capability probes see what they wrote.
        PageControl::Changeable => vec![0xFFu8; BODY_LEN],
    };
    p[4..].copy_from_slice(&body);
    response.extend_from_slice(&p);
}

/// Page 0x0A Subpage 0xF0: Control Data Protection (SPC-4 §7.5.7).
/// 16 bytes total = 4-byte SPF=1 header + 12-byte body.
///
/// Carries Logical Block Protection (LBP) capabilities and host-set
/// runtime state for LTO-7+ E2E CRC32C protection:
///
///   body byte 0 bits 7..5  LBP_W — non-zero: drive accepts WRITE
///                          with WRPROTECT > 0 (4-byte CRC32C
///                          trailer per block).
///   body byte 0 bits 4..2  LBP_R — non-zero: drive returns LBP info
///                          on READ with RDPROTECT > 0.
///   body byte 0 bit 1      RBDP — return LBP info on READ BUFFER
///                          (we don't model READ BUFFER
///                          meaningfully; bit round-trips).
///   body byte 1 bits 4..0  LBP_INFO_LENGTH — protection-info bytes
///                          per block; we advertise 4 (CRC32C).
///   body byte 2            LBP_METHOD — 0x01 = CRC32C only.
///   bytes 3..11            Reserved; emitted as zero.
///
/// Default body is "drive supports CRC32C, host has not enabled LBP."
/// Hosts that enable LBP do so via MODE SELECT writing this page with
/// non-zero LBP_W / LBP_R. The decoded enable bits are read at WRITE
/// / READ time by `core_mediachanger::lbp` consumers in the daemon
/// protocol layer.
fn add_control_data_protection(response: &mut Vec<u8>, pc: PageControl, saved: &DrivePageStore) {
    use core_mediachanger::lbp::LBP_INFO_CRC32C;
    const BODY_LEN: usize = 12;
    let mut p = vec![0u8; 4 + BODY_LEN];
    // PS=1 | SPF=1 | page code 0x0A
    p[0] = 0x80 | 0x40 | 0x0A;
    p[1] = 0xF0; // subpage code
    p[2] = 0x00;
    p[3] = BODY_LEN as u8;

    let body = match pc {
        PageControl::Current | PageControl::Saved => saved
            .get(0x0A, 0xF0)
            .filter(|b| b.len() == BODY_LEN)
            .map(|b| b.to_vec())
            .unwrap_or_else(default_control_data_protection_body),
        PageControl::Default => default_control_data_protection_body(),
        // Changeable mask: LBP_W (bits 7..5) and LBP_R (bits 4..2) of
        // byte 0 are host-tunable. The other fields (LBP_INFO_LENGTH,
        // LBP_METHOD) are fixed advertisements — host can read but not
        // change them. Mark byte 0 as fully changeable for simplicity.
        PageControl::Changeable => {
            let mut mask = vec![0u8; BODY_LEN];
            mask[0] = 0xFE; // bits 7..1 changeable; bit 0 reserved
            mask
        }
    };
    p[4..].copy_from_slice(&body);
    response.extend_from_slice(&p);

    // Reference the constant so it's not flagged as unused if the
    // builder branch above is changed in the future.
    let _ = LBP_INFO_CRC32C;
}

fn default_control_data_protection_body() -> Vec<u8> {
    use core_mediachanger::lbp::LBP_INFO_CRC32C;
    let mut body = vec![0u8; 12];
    // byte 0: LBP_W=0, LBP_R=0 (off by default; host enables via
    // MODE SELECT). RBDP=0.
    body[0] = 0x00;
    // byte 1 bits 4..0: LBP_INFO_LENGTH = 4 (CRC32C trailer width).
    body[1] = 0x04;
    // byte 2: LBP_METHOD = 0x01 (CRC32C).
    body[2] = LBP_INFO_CRC32C;
    body
}

/// Decode host-set Logical Block Protection enable bits from the
/// Mode Page 0x0A/0xF0 saved body. Returns `(write_check, read_check)`.
/// Both default to off when the page hasn't been written or the body
/// is malformed.
pub fn decode_lbp_enables(saved: &DrivePageStore) -> (bool, bool) {
    let Some(body) = saved.get(0x0A, 0xF0) else {
        return (false, false);
    };
    if body.is_empty() {
        return (false, false);
    }
    let lbp_w = (body[0] >> 5) & 0x07;
    let lbp_r = (body[0] >> 2) & 0x07;
    (lbp_w != 0, lbp_r != 0)
}

/// Page 0x1A: Power Condition (SPC-4 §7.5.13). 12 bytes total
/// (2-byte page header + 10-byte body, page-length=0x0A).
///
/// Hosts probe this page to learn how the drive transitions between
/// Active / Idle / Standby states. thurvtl is a virtual drive — power
/// is whatever the host kernel does. We never enter Idle or Standby on
/// our own, and the host has no levers it can pull. Safe defaults:
///   - byte 3 bit 1 (IDLE) = 0   (no auto-idle transition)
///   - byte 3 bit 0 (STANDBY) = 0 (no auto-standby transition)
///   - bytes 4..=7 (Idle Condition Timer, 100 ms units) = 0
///   - bytes 8..=11 (Standby Condition Timer) = 0
///
/// PC=Changeable reports zero — none of these timers are host-tunable
/// on a virtual drive. Default / Saved / Current all read identical.
fn add_power_condition(response: &mut Vec<u8>, pc: PageControl, saved: &DrivePageStore) {
    const DEFAULT: [u8; 10] = [0u8; 10];
    response.push(ps_set(0x1A));
    response.push(0x0A);
    let body = match pc {
        PageControl::Current | PageControl::Saved => saved
            .get(0x1A, 0x00)
            .filter(|b| b.len() == DEFAULT.len())
            .map(|b| b.to_vec())
            .unwrap_or_else(|| DEFAULT.to_vec()),
        PageControl::Default => DEFAULT.to_vec(),
        // Idle/standby flags + timers are spec-tunable. We don't act
        // on them (virtual drive has nothing to power-down), but
        // round-trip is required for conformance.
        PageControl::Changeable => vec![0xFFu8; DEFAULT.len()],
    };
    response.extend_from_slice(&body);
}

/// Page 0x1C: Informational Exceptions Control (SPC-5 §7.5.10)
///
/// Standard SPC page that backup software polls to learn how the drive
/// reports informational exceptions (predictive failure, threshold
/// excursions, TapeAlert events). 12 bytes total (4-byte header + 8-byte
/// body, expressed as page-length=0x0A on the SCSI wire).
///
/// thurvtl is a virtual drive — it never raises informational
/// exceptions, so the safest defaults are:
///   - DExcpt=0 (exception generation enabled, but nothing to generate)
///   - MRIE=0x06 ("Only Report on Request") — least intrusive, matches
///     what most LTO drives ship with by default. Hosts that want
///     TapeAlert can poll LOG SENSE 0x2E directly.
///   - Interval Timer / Report Count = 0
///
/// PC=Changeable reports zero — none of these fields are host-tunable.
fn add_informational_exceptions_control(
    response: &mut Vec<u8>,
    pc: PageControl,
    saved: &DrivePageStore,
) {
    const DEFAULT: [u8; 10] = [
        0x00, // PERF / EBF / EWasc / DExcpt / TEST / EBackErr / LogErr
        0x06, // MRIE = 6 (Only Report on Request)
        0x00, 0x00, 0x00, 0x00, // Interval Timer
        0x00, 0x00, 0x00, 0x00, // Report Count
    ];
    response.push(ps_set(0x1C));
    response.push(0x0A);
    let body = match pc {
        PageControl::Current | PageControl::Saved => saved
            .get(0x1C, 0x00)
            .filter(|b| b.len() == DEFAULT.len())
            .map(|b| b.to_vec())
            .unwrap_or_else(|| DEFAULT.to_vec()),
        PageControl::Default => DEFAULT.to_vec(),
        // Every field except the reserved bits is host-tunable per
        // SPC-5. Round-trip is what matters; the drive doesn't actually
        // raise informational exceptions.
        PageControl::Changeable => vec![
            0xFD, // PERF/EBF/EWasc/DExcpt/TEST/EBackErr/LogErr (bit 0 reserved)
            0x0F, // MRIE (low 4 bits)
            0xFF, 0xFF, 0xFF, 0xFF, // Interval Timer
            0xFF, 0xFF, 0xFF, 0xFF, // Report Count
        ],
    };
    response.extend_from_slice(&body);
}

/// Page 0x11: Medium Partition (SSC-4 §8.3.4.4). LTFS uses this page to
/// describe the two-partition layout (Index Partition P0 + Data Partition
/// P1) it wants the drive to apply on the next FORMAT MEDIUM.
///
/// We always advertise:
///   max_additional_partitions = 1   (LTO supports 2 partitions total)
///   PSUM = MiB                      (size unit, code 2)
///   FDP / SDP / IDP changeable      (so MODE SELECT(0x11) is honoured)
///
/// On a Current/Default/Saved read we mirror the *current* tape state:
/// `additional_partitions = partition_count - 1`. That way an unpartitioned
/// tape correctly reports 0 additional partitions, and a tape that has been
/// formatted with LTFS reports 1.
fn add_medium_partition(response: &mut Vec<u8>, pc: PageControl, snapshot: PartitionSnapshot) {
    let page_code = ps_set(0x11);
    // SSC-4 medium partition page is variable length:
    //   byte 0: page code
    //   byte 1: page length (= total length - 2)
    //   byte 2: max additional partitions
    //   byte 3: additional partitions defined
    //   byte 4: FDP/SDP/IDP/PSUM/POFM/CLEAR/ADDP bits
    //   byte 5: medium format recognition
    //   byte 6: partition units (PSUM low nibble = 2 means MiB)
    //   byte 7: reserved
    //   bytes 8..: partition size descriptors (2 bytes each, big-endian)
    // We emit two size descriptors so a host that forgets to issue MODE SELECT
    // before FORMAT MEDIUM still gets a sensible default layout.
    let page_length: u8 = 0x0A;
    response.push(page_code);
    response.push(page_length);

    let additional_partitions = if snapshot.partition_count == 0 {
        0
    } else {
        snapshot.partition_count - 1
    };

    match pc {
        PageControl::Current | PageControl::Default | PageControl::Saved => {
            response.push(0x01); // Max additional partitions = 1 (= 2 partitions total)
            response.push(additional_partitions); // Currently defined
            // FDP=0, SDP=0, IDP=1 (initiator-defined sizes accepted),
            // PSUM=2 (MiB) shifted into bits 5..4. POFM=0 CLEAR=0 ADDP=0.
            response.push(0b0010_0000 | 0b0010);
            response.push(0x00); // Medium format recognition
            response.push(0x00); // Partition units (legacy field; PSUM above is authoritative)
            response.push(0x00); // Reserved
            // Two size descriptors, both 0xFFFF = "rest of tape" placeholder.
            // Real LTFS overrides via MODE SELECT(0x11) before issuing FORMAT MEDIUM.
            response.extend_from_slice(&[0xFF, 0xFF]);
            response.extend_from_slice(&[0xFF, 0xFF]);
        }
        PageControl::Changeable => {
            // FDP, SDP, IDP, partition sizes all changeable.
            response.push(0x01); // Max additional partitions
            response.push(0xFF); // Additional partitions changeable
            response.push(0xFF); // FDP/SDP/IDP/PSUM all changeable
            response.push(0x00);
            response.push(0x00);
            response.push(0x00);
            response.extend_from_slice(&[0xFF, 0xFF]);
            response.extend_from_slice(&[0xFF, 0xFF]);
        }
    }
}

/// Parse the body of a MODE SELECT page 0x11 (everything past the
/// 2-byte page header). Body byte 0 is `max additional partitions`,
/// byte 1 is `additional partitions defined`, byte 2 carries the
/// FDP/SDP/IDP flags + PSUM, and bytes 6.. are partition size
/// descriptors (2 bytes each, big-endian).
fn parse_medium_partition_page(body: &[u8]) -> Result<PendingPartitionLayout, String> {
    if body.len() < 6 {
        return Err("MODE SELECT page 0x11: too short".to_string());
    }
    let additional_partitions = body[1];
    let flags = body[2];
    let fdp = (flags & 0x80) != 0; // bit 7
    let sdp = (flags & 0x40) != 0; // bit 6
    let idp = (flags & 0x20) != 0; // bit 5
    let psum = (flags >> 3) & 0x03; // bits 4..3
    // Size descriptors start at body[6], 2 bytes each.
    let mut sizes: Vec<u64> = Vec::new();
    let mut off = 6usize;
    while off + 2 <= body.len() {
        let v = u16::from_be_bytes([body[off], body[off + 1]]) as u64;
        sizes.push(v);
        off += 2;
    }
    // Total partitions on the medium = 1 (P0) + additional. We expect at
    // least that many size descriptors for an IDP layout.
    if idp && sizes.len() < (1 + additional_partitions as usize) {
        return Err(format!(
            "MODE SELECT page 0x11: IDP set but {} size descriptors for {} partitions",
            sizes.len(),
            1 + additional_partitions as usize
        ));
    }
    Ok(PendingPartitionLayout {
        fdp,
        sdp,
        idp,
        additional_partitions,
        psum,
        partition_sizes: sizes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(n: u8) -> PartitionSnapshot {
        PartitionSnapshot { partition_count: n }
    }

    fn comp_off() -> CompressionSnapshot {
        CompressionSnapshot { dce: false }
    }

    fn comp_on() -> CompressionSnapshot {
        CompressionSnapshot { dce: true }
    }

    fn empty_saved() -> DrivePageStore {
        DrivePageStore::default()
    }

    #[test]
    fn test_mode_sense_control_data_protection_default() {
        // Page 0x0A subpage 0xF0 with no saved body advertises CRC32C
        // capability (LBP_INFO_LENGTH=4, LBP_METHOD=0x01) and LBP off.
        let result = handle_mode_sense_6(
            0x0A,
            0xF0,
            PageControl::Current,
            true,
            snap(1),
            comp_off(),
            &empty_saved(),
        );
        let data = result.expect("MODE SENSE 0x0A/0xF0 should succeed");
        // 4-byte header (mode parameter header) + 4-byte page header
        // + 12-byte body = 20 bytes total.
        assert_eq!(data.len(), 20);
        // page header: byte 0 = PS|SPF|0x0A = 0xCA
        assert_eq!(data[4], 0xCA);
        // subpage code 0xF0
        assert_eq!(data[5], 0xF0);
        // page length BE = 12
        assert_eq!(u16::from_be_bytes([data[6], data[7]]), 12);
        // body byte 0 = 0 (LBP off)
        assert_eq!(data[8], 0x00);
        // body byte 1 (LBP_INFO_LENGTH) = 4
        assert_eq!(data[9] & 0x1F, 0x04);
        // body byte 2 (LBP_METHOD) = 0x01 (CRC32C)
        assert_eq!(data[10], 0x01);
    }

    #[test]
    fn test_decode_lbp_enables_off_by_default() {
        let saved = empty_saved();
        assert_eq!(decode_lbp_enables(&saved), (false, false));
    }

    #[test]
    fn test_decode_lbp_enables_after_host_set() {
        let mut saved = DrivePageStore::default();
        // body byte 0: LBP_W=1 (bits 7..5 = 0b001 → 0x20),
        //              LBP_R=1 (bits 4..2 = 0b001 → 0x04)
        let mut body = vec![0u8; 12];
        body[0] = 0x20 | 0x04;
        saved.set(0x0A, 0xF0, body);
        assert_eq!(decode_lbp_enables(&saved), (true, true));
    }

    #[test]
    fn test_decode_lbp_enables_write_only() {
        let mut saved = DrivePageStore::default();
        let mut body = vec![0u8; 12];
        body[0] = 0x20; // LBP_W only
        saved.set(0x0A, 0xF0, body);
        assert_eq!(decode_lbp_enables(&saved), (true, false));
    }

    #[test]
    fn test_mode_sense_6_error_recovery() {
        let result = handle_mode_sense_6(
            0x01,
            0x00,
            PageControl::Current,
            true,
            snap(1),
            comp_off(),
            &empty_saved(),
        );
        assert!(result.is_ok());
        let data = result.unwrap();

        // Header (4 bytes) + Page header (2 bytes) + Page data (10 bytes) = 16 bytes
        assert_eq!(data.len(), 16);
        assert_eq!(data[0], 15); // Mode data length (total - 1)
        assert_eq!(data[4] & 0x3F, 0x01); // page code (PS bit may be set)
        assert_eq!(data[5], 0x0A); // Page length
    }

    #[test]
    fn test_mode_sense_6_all_pages() {
        let result = handle_mode_sense_6(
            0x3F,
            0x00,
            PageControl::Current,
            true,
            snap(1),
            comp_off(),
            &empty_saved(),
        );
        assert!(result.is_ok());
        let data = result.unwrap();

        // Should contain all four pages
        assert!(data.len() > 50);
        assert_eq!(data[4] & 0x3F, 0x01); // First page: error recovery
    }

    #[test]
    fn test_mode_sense_10_compression() {
        let result = handle_mode_sense_10(
            0x0F,
            0x00,
            PageControl::Current,
            true,
            false,
            snap(1),
            comp_off(),
            &empty_saved(),
        );
        assert!(result.is_ok());
        let data = result.unwrap();

        // Header (8 bytes) + Page (16 bytes) = 24 bytes
        assert_eq!(data.len(), 24);
        assert_eq!(data[8] & 0x3F, 0x0F); // Page code (PS bit may be set)
        assert_eq!(data[9], 0x0E); // Page length
        // DCC always 1, DCE off when comp_off()
        assert_eq!(data[10] & 0xC0, 0x40, "DCE=0, DCC=1 expected");
        // DDE always 1
        assert_eq!(data[11] & 0x80, 0x80, "DDE=1 expected");
    }

    #[test]
    fn test_mode_sense_compression_dce_on() {
        let data = handle_mode_sense_6(
            0x0F,
            0x00,
            PageControl::Current,
            true,
            snap(1),
            comp_on(),
            &empty_saved(),
        )
        .unwrap();
        // page header at 4..6, body at 6..
        assert_eq!(data[4] & 0x3F, 0x0F);
        // DCE=1, DCC=1
        assert_eq!(data[6] & 0xC0, 0xC0, "DCE=1, DCC=1 expected");
    }

    #[test]
    fn test_mode_sense_compression_default_pc() {
        // PC=Default reports DCE=0 regardless of runtime state — that's
        // the spec'd factory default.
        let data = handle_mode_sense_6(
            0x0F,
            0x00,
            PageControl::Default,
            true,
            snap(1),
            comp_on(),
            &empty_saved(),
        )
        .unwrap();
        assert_eq!(data[4] & 0x3F, 0x0F);
        assert_eq!(data[6] & 0xC0, 0x40, "Default PC: DCE=0, DCC=1 expected");
    }

    #[test]
    fn test_mode_sense_partition_page_unpartitioned() {
        let data = handle_mode_sense_6(
            0x11,
            0x00,
            PageControl::Current,
            true,
            snap(1),
            comp_off(),
            &empty_saved(),
        )
        .unwrap();
        // header(4) + page header(2) + page body(10)
        assert_eq!(data.len(), 16);
        assert_eq!(data[4] & 0x3F, 0x11);
        assert_eq!(data[5], 0x0A);
        assert_eq!(data[6], 0x01); // max additional = 1
        assert_eq!(data[7], 0x00); // additional defined = 0 on a fresh tape
    }

    #[test]
    fn test_mode_sense_partition_page_two_partitions() {
        let data = handle_mode_sense_6(
            0x11,
            0x00,
            PageControl::Current,
            true,
            snap(2),
            comp_off(),
            &empty_saved(),
        )
        .unwrap();
        assert_eq!(data[7], 0x01); // additional defined = 1 after LTFS format
    }

    #[test]
    fn test_mode_sense_power_condition() {
        let data = handle_mode_sense_6(
            0x1A,
            0x00,
            PageControl::Current,
            true,
            snap(1),
            comp_off(),
            &empty_saved(),
        )
        .unwrap();
        // header(4) + page header(2) + page body(10)
        assert_eq!(data.len(), 16);
        assert_eq!(data[4] & 0x3F, 0x1A);
        assert_eq!(data[5], 0x0A);
        // Body all zeros — no auto-idle, no auto-standby.
        assert!(
            data[6..].iter().all(|&b| b == 0),
            "page 0x1A body should be all zeros on a virtual drive"
        );
    }

    #[test]
    fn test_mode_sense_all_pages_includes_0x1a() {
        let data = handle_mode_sense_6(
            0x3F,
            0x00,
            PageControl::Current,
            true,
            snap(1),
            comp_off(),
            &empty_saved(),
        )
        .unwrap();
        let mut i = 4;
        let mut found = false;
        while i + 2 <= data.len() {
            let code = data[i] & 0x3F;
            let len = data[i + 1] as usize;
            if code == 0x1A {
                found = true;
                break;
            }
            i += 2 + len;
        }
        assert!(found, "page 0x1A missing from MODE SENSE all-pages");
    }

    #[test]
    fn test_mode_sense_disconnect_reconnect() {
        let data = handle_mode_sense_6(
            0x02,
            0x00,
            PageControl::Current,
            true,
            snap(1),
            comp_off(),
            &empty_saved(),
        )
        .unwrap();
        // header(4) + page header(2) + page body(14)
        assert_eq!(data.len(), 20);
        assert_eq!(data[4] & 0x3F, 0x02);
        assert_eq!(data[5], 0x0E);
        // Body all zeros — spec'd "transport doesn't model disconnect" answer.
        assert!(
            data[6..].iter().all(|&b| b == 0),
            "page 0x02 body should be all zeros on iSCSI"
        );
    }

    #[test]
    fn test_mode_sense_all_pages_includes_0x02() {
        let data = handle_mode_sense_6(
            0x3F,
            0x00,
            PageControl::Current,
            true,
            snap(1),
            comp_off(),
            &empty_saved(),
        )
        .unwrap();
        let mut i = 4;
        let mut found = false;
        while i + 2 <= data.len() {
            let code = data[i] & 0x3F;
            let len = data[i + 1] as usize;
            if code == 0x02 {
                found = true;
                break;
            }
            i += 2 + len;
        }
        assert!(found, "page 0x02 missing from MODE SENSE all-pages");
    }

    #[test]
    fn test_mode_sense_informational_exceptions_control() {
        let data = handle_mode_sense_6(
            0x1C,
            0x00,
            PageControl::Current,
            true,
            snap(1),
            comp_off(),
            &empty_saved(),
        )
        .unwrap();
        // header(4) + page header(2) + page body(10)
        assert_eq!(data.len(), 16);
        assert_eq!(data[4] & 0x3F, 0x1C);
        assert_eq!(data[5], 0x0A);
        // byte 0 of body: all flag bits zero (DExcpt=0, etc.)
        assert_eq!(data[6], 0x00);
        // byte 1: MRIE = 6 (Only Report on Request)
        assert_eq!(data[7] & 0x0F, 0x06);
    }

    #[test]
    fn test_mode_sense_all_pages_includes_0x1c() {
        let data = handle_mode_sense_6(
            0x3F,
            0x00,
            PageControl::Current,
            true,
            snap(1),
            comp_off(),
            &empty_saved(),
        )
        .unwrap();
        // Walk pages and check 0x1C appears.
        let mut i = 4;
        let mut found = false;
        while i + 2 <= data.len() {
            let code = data[i] & 0x3F;
            let len = data[i + 1] as usize;
            if code == 0x1C {
                found = true;
                break;
            }
            i += 2 + len;
        }
        assert!(found, "page 0x1C missing from MODE SENSE all-pages");
    }

    #[test]
    fn test_mode_select_6_empty() {
        let result = handle_mode_select_6(false, &[]);
        assert!(result.is_ok());
        assert!(result.unwrap().pending_layout.is_none());
    }

    #[test]
    fn test_mode_select_6_valid() {
        // Minimal valid MODE SELECT data: header only
        let data = vec![0x00, 0x00, 0x00, 0x00]; // Mode header, no block desc, no pages
        let result = handle_mode_select_6(false, &data);
        assert!(result.is_ok());
        assert!(result.unwrap().pending_layout.is_none());
    }

    #[test]
    fn test_mode_select_compression_dce_on() {
        // 4-byte mode header + page 0x0F with 14-byte body, DCE=1.
        let mut data = vec![0x00, 0x00, 0x00, 0x00, 0x0F, 0x0E];
        data.push(0x80); // DCE=1
        data.extend_from_slice(&[0u8; 13]);
        let outcome = handle_mode_select_6(false, &data).unwrap();
        assert_eq!(outcome.compression_dce, Some(true));
    }

    #[test]
    fn test_mode_select_compression_dce_off() {
        let mut data = vec![0x00, 0x00, 0x00, 0x00, 0x0F, 0x0E];
        data.push(0x00); // DCE=0
        data.extend_from_slice(&[0u8; 13]);
        let outcome = handle_mode_select_6(false, &data).unwrap();
        assert_eq!(outcome.compression_dce, Some(false));
    }

    #[test]
    fn test_mode_sense_6_device_config_extension() {
        // Page 0x10 / Subpage 0x01 — 4-byte mode header + 32-byte page
        // (4-byte SPF=1 page header + 28-byte body of zeros).
        let data = handle_mode_sense_6(
            0x10,
            0x01,
            PageControl::Current,
            true,
            snap(1),
            comp_off(),
            &empty_saved(),
        )
        .unwrap();
        assert_eq!(data.len(), 4 + 32, "header + page");
        // PS=1 (we save) | SPF=1 (subpage) | page code 0x10
        assert_eq!(data[4], 0x80 | 0x40 | 0x10, "PS=1 + SPF=1 + page code 0x10");
        assert_eq!(data[5], 0x01, "subpage code");
        assert_eq!(
            u16::from_be_bytes([data[6], data[7]]),
            0x001C,
            "page length 28"
        );
        // Body is all zeros (no append-only, no encrypt-only modeled).
        assert!(
            data[8..].iter().all(|&b| b == 0),
            "body should be all zeros"
        );
    }

    #[test]
    fn test_mode_sense_6_subpage_0xff_includes_dev_config_ext() {
        // page_code=0x3F + subpage_code=0xFF → all SPF=0 pages plus all
        // subpages, so the Device Configuration Extension subpage must be
        // emitted as the last page.
        let data = handle_mode_sense_6(
            0x3F,
            0xFF,
            PageControl::Current,
            true,
            snap(1),
            comp_off(),
            &empty_saved(),
        )
        .unwrap();
        // Walk pages and look for byte sequence 0x50, 0x01 (SPF=1 page 0x10
        // subpage 0x01).
        let mut found = false;
        for i in 4..data.len().saturating_sub(1) {
            if (data[i] & 0x7F) == (0x40 | 0x10) && data[i + 1] == 0x01 {
                found = true;
                break;
            }
        }
        assert!(found, "page 0x10/01 should appear with subpage_code=0xFF");
    }

    #[test]
    fn test_mode_sense_6_partition_page_subpage_0x01_omitted() {
        // page_code=0x11 + subpage_code=0x01 should only emit the subpage
        // 0x01 (Device Configuration Extension is the only subpage we
        // model). For page 0x11 specifically, no subpage 0x01 exists.
        // Result: response has the 4-byte header but no page data.
        let data = handle_mode_sense_6(
            0x11,
            0x01,
            PageControl::Current,
            true,
            snap(1),
            comp_off(),
            &empty_saved(),
        )
        .unwrap();
        assert_eq!(data.len(), 4, "header only — page 0x11 has no subpage 0x01");
    }

    #[test]
    fn test_mode_select_records_saved_pages() {
        // Two pages in one parameter list: 0x01 (10-byte body) and
        // 0x1C (10-byte body). Both should appear in saved_pages.
        let mut data = vec![0x00, 0x00, 0x00, 0x00]; // mode header
        data.extend_from_slice(&[0x01, 0x0A]); // page 0x01 header
        let p01_body = [0x88, 0x05, 0, 0, 0, 0, 0x88, 0x05, 0x12, 0x34];
        data.extend_from_slice(&p01_body);
        data.extend_from_slice(&[0x1C, 0x0A]); // page 0x1C header
        let p1c_body = [0xC8, 0x04, 0, 0, 0x10, 0, 0, 0, 0, 5];
        data.extend_from_slice(&p1c_body);

        let outcome = handle_mode_select_6(true, &data).unwrap();
        assert!(outcome.save_pages, "SP=true should propagate");
        assert_eq!(outcome.saved_pages.len(), 2);
        assert_eq!(outcome.saved_pages[0], (0x01, 0x00, p01_body.to_vec()));
        assert_eq!(outcome.saved_pages[1], (0x1C, 0x00, p1c_body.to_vec()));
    }

    #[test]
    fn test_mode_select_spf1_subpage_parsed() {
        // 4-byte mode header + page 0x10/0x01 with SPF=1 4-byte page
        // header (page_code|0x40, subpage_code, len BE16).
        let mut data = vec![0x00, 0x00, 0x00, 0x00];
        data.extend_from_slice(&[0x40 | 0x10, 0x01, 0x00, 0x1C]);
        data.extend_from_slice(&[0u8; 28]);

        let outcome = handle_mode_select_6(false, &data).unwrap();
        assert_eq!(outcome.saved_pages.len(), 1);
        let (pc, sp, body) = &outcome.saved_pages[0];
        assert_eq!(*pc, 0x10);
        assert_eq!(*sp, 0x01);
        assert_eq!(body.len(), 28);
    }

    #[test]
    fn test_mode_sense_replays_saved_body() {
        // Host-written body for page 0x1C with MRIE=4 (Recovered Error)
        // and a non-zero Interval Timer must come back on the next
        // PC=Current MODE SENSE.
        let mut state = DrivePageStore::default();
        let saved_body: [u8; 10] = [0x80, 0x04, 0x00, 0x00, 0x12, 0x34, 0x00, 0x00, 0x00, 0x07];
        state.set(0x1C, 0x00, saved_body.to_vec());

        let data = handle_mode_sense_6(
            0x1C,
            0x00,
            PageControl::Current,
            true,
            snap(1),
            comp_off(),
            &state,
        )
        .unwrap();
        assert_eq!(data.len(), 16);
        assert_eq!(data[4] & 0x3F, 0x1C);
        assert_eq!(
            &data[6..16],
            &saved_body,
            "saved body must be replayed verbatim"
        );
    }

    #[test]
    fn test_mode_sense_compression_runtime_dce_overrides_saved() {
        // Even if a saved body has DCE=0, MODE SENSE PC=Current must
        // emit the runtime DCE bit — that's the truth of the drive.
        let mut state = DrivePageStore::default();
        let mut body = vec![
            0x40, 0x80, // DCE=0 | DCC=1, DDE=1
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        body[1] = 0x80; // pretend host wrote DDE=1, RED=0
        state.set(0x0F, 0x00, body);

        let data = handle_mode_sense_6(
            0x0F,
            0x00,
            PageControl::Current,
            true,
            snap(1),
            comp_on(),
            &state,
        )
        .unwrap();
        // DCE must be on regardless of saved body's DCE bit.
        assert_eq!(data[6] & 0xC0, 0xC0, "runtime DCE=1 wins");
    }

    #[test]
    fn test_mode_sense_ps_bit_set() {
        // Every page header byte must carry PS=1 — the drive saves.
        let data = handle_mode_sense_6(
            0x01,
            0x00,
            PageControl::Current,
            true,
            snap(1),
            comp_off(),
            &empty_saved(),
        )
        .unwrap();
        assert_eq!(data[4] & 0x80, 0x80, "PS=1 expected on page 0x01");
    }

    #[test]
    fn test_mode_sense_changeable_mask_advertises_round_trip() {
        // PC=Changeable on page 0x1C should advertise MRIE + Interval
        // Timer + Report Count + most byte-0 flags as host-tunable.
        let data = handle_mode_sense_6(
            0x1C,
            0x00,
            PageControl::Changeable,
            true,
            snap(1),
            comp_off(),
            &empty_saved(),
        )
        .unwrap();
        assert_eq!(data[7], 0x0F, "MRIE nibble must be changeable");
        assert!(
            data[8..12].iter().all(|&b| b == 0xFF),
            "Interval Timer changeable"
        );
        assert!(
            data[12..16].iter().all(|&b| b == 0xFF),
            "Report Count changeable"
        );
    }

    #[test]
    fn test_mode_select_6_partition_page() {
        // 4-byte mode header + page 0x11 with 0x0A page length, IDP=1 PSUM=2 (MiB),
        // additional_partitions=1, two size descriptors (1024 MiB, 0xFFFF=rest).
        let data = vec![
            // mode header (4 bytes)
            0x00,
            0x00,
            0x00,
            0x00, //
            // page 0x11 header
            0x11,
            0x0A, //
            // body
            0x01,                      // max additional partitions
            0x01,                      // additional partitions defined
            0b0010_0000 | 0b0001_0000, // IDP=1 (bit5), PSUM=2 (bits 4..3 = 10)
            0x00,                      // medium format recognition
            0x00,                      // partition units (legacy)
            0x00,                      // reserved
            0x04,
            0x00, // partition 0 size = 1024 MiB
            0xFF,
            0xFF, // partition 1 size = "rest of tape"
        ];
        let outcome = handle_mode_select_6(false, &data).unwrap();
        let layout = outcome
            .pending_layout
            .expect("page 0x11 should be parsed out");
        assert!(!layout.fdp);
        assert!(!layout.sdp);
        assert!(layout.idp);
        assert_eq!(layout.additional_partitions, 1);
        assert_eq!(layout.psum, 2); // MiB
        assert_eq!(layout.partition_sizes, vec![1024, 0xFFFF]);
    }

    #[test]
    fn mode_sense_10_llbaa_emits_16_byte_long_block_descriptor() {
        // Issue #98: LLBAA=1 must declare a 16-byte long-LBA block
        // descriptor in the header AND emit exactly that many bytes (the
        // old stub declared 8 but emitted 32 zeros).
        let data = handle_mode_sense_10(
            0x0A,
            0xF0,
            PageControl::Current,
            false, // dbd
            true,  // llbaa
            snap(1),
            comp_off(),
            &empty_saved(),
        )
        .expect("MODE SENSE(10) LLBAA=1 should succeed");

        // Header byte 4: LONGLBA bit set.
        assert_eq!(data[4] & 0x01, 0x01, "LONGLBA bit set");
        // Header bytes 6-7: declared block-descriptor length == 16.
        assert_eq!(
            u16::from_be_bytes([data[6], data[7]]),
            16,
            "declared long-LBA block-descriptor length"
        );
        // The 16 descriptor bytes — variable-block tape: all zero.
        assert_eq!(
            &data[8..24],
            &[0u8; 16],
            "long-LBA descriptor is variable-block zeros"
        );
        // Page 0x0A header (PS|SPF|0x0A = 0xCA) begins exactly after the
        // 16-byte descriptor — proves emitted length == declared length.
        assert_eq!(data[24], 0xCA, "page header follows the 16-byte descriptor");
        // MODE DATA LENGTH (bytes 0-1) accounts for the whole response.
        assert_eq!(
            u16::from_be_bytes([data[0], data[1]]) as usize,
            data.len() - 2,
            "MODE DATA LENGTH = total - 2"
        );
    }

    #[test]
    fn mode_sense_10_short_descriptor_unchanged() {
        // LLBAA=0 regression guard: 8-byte short descriptor, page at
        // offset 8 + 8 = 16.
        let data = handle_mode_sense_10(
            0x0A,
            0xF0,
            PageControl::Current,
            false,
            false,
            snap(1),
            comp_off(),
            &empty_saved(),
        )
        .expect("MODE SENSE(10) LLBAA=0 should succeed");
        assert_eq!(data[4] & 0x01, 0x00, "LONGLBA bit clear");
        assert_eq!(
            u16::from_be_bytes([data[6], data[7]]),
            8,
            "short block-descriptor length"
        );
        assert_eq!(data[16], 0xCA, "page header follows the 8-byte descriptor");
    }
}
