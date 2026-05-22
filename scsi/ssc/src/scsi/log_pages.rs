// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// LOG SENSE implementation for SSC-2 tape drives
// Reference: SCSI Stream Commands (SSC-2) specification
//
// Complete SCSI log page implementation per spec.

#![allow(dead_code)]

/// Log page codes for tape drives (SSC-2)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LogPageCode {
    SupportedLogPages = 0x00,
    WriteErrors = 0x02,
    ReadErrors = 0x03,
    ReadReverseErrors = 0x04,
    NonMediumErrors = 0x06,
    SequentialAccessDevice = 0x0C,
    Temperature = 0x0D,
    DtDeviceStatus = 0x11,
    TapeAlertResponse = 0x12,
    DeviceStatistics = 0x14,
    LastNErrorEvents = 0x16,
    VolumeStatistics = 0x17,
    PowerConditionTransitions = 0x1A,
    DataCompression = 0x1B,
    TapeUsageDeprecated = 0x30,
    TapeCapacity = 0x31,
    DataCompressionDeprecated = 0x32,
}

/// Handle LOG SENSE for the medium changer (LUN 0). The changer
/// advertises a deliberately narrow set: Supported (0x00), Temperature
/// (0x0D), and TapeAlert (0x2E). Vendor-specific log pages (vendor
/// event / statistics / error / device-status pages in the 0xC0-0xFF
/// range) are not implemented — backup software polls 0x00 + 0x0D +
/// 0x2E.
pub fn handle_changer_log_sense(
    page_code: u8,
    subpage_code: u8,
    pc: u8,
) -> Result<Vec<u8>, String> {
    tracing::debug!(
        "LOG SENSE (changer): page=0x{:02x} sub=0x{:02x} PC={}",
        page_code,
        subpage_code,
        pc
    );

    let mut response = vec![page_code, subpage_code, 0x00, 0x00];

    match page_code {
        0x00 => {
            for &p in &[0x00u8, 0x0D, 0x2E] {
                add_log_parameter(&mut response, u16::from(p), &[p]);
            }
        }
        0x0D => add_temperature_log(&mut response),
        0x2E => add_tape_alert_log(&mut response),
        _ => return Err(format!("Unsupported changer log page: 0x{:02x}", page_code)),
    }

    let page_len = (response.len() - 4) as u16;
    response[2] = (page_len >> 8) as u8;
    response[3] = (page_len & 0xFF) as u8;
    Ok(response)
}

/// Handle LOG SENSE command (0x4D)
/// Returns log page data for statistics and diagnostics
pub fn handle_log_sense(
    page_code: u8,
    subpage_code: u8,
    pc: u8, // Page Control (0 = threshold, 1 = cumulative, 2 = default, 3 = current threshold)
    mfg_serial: &str,
) -> Result<Vec<u8>, String> {
    tracing::debug!(
        "LOG SENSE: page_code=0x{:02x}, subpage=0x{:02x}, PC={}",
        page_code,
        subpage_code,
        pc
    );

    // Log page header (4 bytes): page code, subpage code, length MSB,
    // length LSB. The length is patched in once the body is appended.
    let mut response = vec![page_code, subpage_code, 0x00, 0x00];

    match page_code {
        0x00 => {
            // Supported Log Pages
            add_supported_log_pages(&mut response);
        }
        0x02 => {
            // Write Error Counter
            add_write_error_counters(&mut response);
        }
        0x03 => {
            // Read Error Counter
            add_read_error_counters(&mut response);
        }
        0x06 => {
            // Non-Medium Error Counter
            add_non_medium_error_counters(&mut response);
        }
        0x0C => {
            // Sequential Access Device (SSC-5 §8.5; supersedes the legacy
            // 0x30 Tape Usage page). Bytes-transferred counters and
            // partition-capacity hints. Virtual drive — counters all zero,
            // capacity hints zero (we don't model partition capacity yet).
            add_sequential_access_device_log(&mut response);
        }
        0x0D => {
            // Temperature
            add_temperature_log(&mut response);
        }
        0x11 => {
            // DT Device Status (named TapeUsage in the enum for historical reasons).
            add_dt_device_status_log(&mut response);
        }
        0x12 => {
            // Tape Alert Response (SSC-3 §8.2.4) — host poll-side companion
            // to the 0x2E TapeAlert page. Reports the sequence number of the
            // most-recently-raised event and which flags are currently set.
            // A virtual drive with no alert history returns an empty
            // parameter list.
            add_tape_alert_response_log(&mut response);
        }
        0x14 => {
            // Device Statistics (SSC-5 §8.5). Lifetime counters that backup
            // software polls for drive-health reporting. Virtual drive — every
            // counter reads zero.
            add_device_statistics_log(&mut response, mfg_serial);
        }
        0x16 => {
            // Last n Error Events (SSC-5 §8.6). Vendor-recorded error history;
            // an empty parameter list is a valid response and matches what a
            // drive with no errors would report.
            add_last_n_error_events_log(&mut response);
        }
        0x17 => {
            // Volume Statistics (SSC-5 §8.7). Per-mounted-volume counters.
            // Validity=0 with all-zero counters is the correct shape when no
            // volume is mounted; the same shape is also acceptable for a
            // mounted volume on a virtual drive that doesn't track these.
            add_volume_statistics_log(&mut response);
        }
        0x1A => {
            // Power Condition Transitions (SPC-4 §7.3.16). Cumulative count
            // of transitions into each power state. Virtual drive — never
            // transitions, every counter zero.
            add_power_condition_transitions_log(&mut response);
        }
        0x1B => {
            // Data Compression (SSC-5; replaces the deprecated 0x32). Read /
            // Write compression ratios + cumulative bytes transferred. We
            // report 1:1 ratio (0x0100 = 1.00) since the SCSI surface doesn't
            // show drive-level compression as a ratio change — block payloads
            // are decompressed before MODE SENSE / LOG SENSE see them. All
            // byte counters zero on the virtual drive.
            add_data_compression_log(&mut response);
        }
        0x30 => {
            // Tape Usage (legacy; deprecated by SSC-5 in favor of
            // 0x14 Device Statistics + 0x0C Sequential Access Device).
            // Legacy backup software (older NetBackup, Bareos) still polls
            // this page during drive-capability detection. Counters all
            // zero on a virtual drive.
            add_tape_usage_legacy_log(&mut response);
        }
        0x31 => {
            // Tape Capacity (legacy). Per-partition remaining /
            // maximum capacity in MB. Legacy companion to 0x0C Sequential
            // Access Device. thurvtl doesn't model partition capacity
            // yet — every counter zero (the same shape 0x0C reports).
            add_tape_capacity_legacy_log(&mut response);
        }
        0x32 => {
            // Data Compression (legacy; deprecated by SSC-5 in
            // favor of 0x1B). Same per-counter shape as 0x1B but with the
            // older parameter codes some pre-LTO-7 backup software keys on.
            // 1:1 ratio (0x0100), all byte counters zero — mirrors 0x1B.
            add_data_compression_legacy_log(&mut response);
        }
        0x2E => {
            // TapeAlert (SSC-3 / TapeAlert spec) — 64 boolean flags, one per
            // parameter. Each parameter is `flag_index` (1..=64) with one byte
            // of payload (the flag value). Backup software (Veeam, NetBackup,
            // Bareos) polls this page to surface drive/media health.
            add_tape_alert_log(&mut response);
        }
        _ => {
            return Err(format!("Unsupported log page: 0x{:02x}", page_code));
        }
    }

    // Update page length (total - header)
    let page_len = (response.len() - 4) as u16;
    response[2] = (page_len >> 8) as u8;
    response[3] = (page_len & 0xFF) as u8;

    tracing::debug!("LOG SENSE response: {} bytes", response.len());
    Ok(response)
}

// ============================================================================
// Log Page Builders
// ============================================================================

/// Page 0x00: Supported Log Pages
fn add_supported_log_pages(response: &mut Vec<u8>) {
    // List of supported log pages
    let supported = [
        0x00, 0x02, 0x03, 0x06, 0x0C, 0x0D, 0x11, 0x12, 0x14, 0x16, 0x17, 0x1A, 0x1B, 0x2E, 0x30,
        0x31, 0x32,
    ];

    for &page in &supported {
        // Each entry is a log parameter (parameter code 0x0000 + sequential index)
        add_log_parameter(response, page as u16, &[page]);
    }
}

/// Page 0x02: Write Error Counters
fn add_write_error_counters(response: &mut Vec<u8>) {
    // Parameter 0x0000: Errors corrected without substantial delay
    add_counter_parameter(response, 0x0000, 0);

    // Parameter 0x0001: Errors corrected with possible delays
    add_counter_parameter(response, 0x0001, 0);

    // Parameter 0x0002: Total rewrites or rereads
    add_counter_parameter(response, 0x0002, 0);

    // Parameter 0x0003: Total errors corrected
    add_counter_parameter(response, 0x0003, 0);

    // Parameter 0x0004: Total times correction algorithm processed
    add_counter_parameter(response, 0x0004, 0);

    // Parameter 0x0005: Total bytes processed
    add_counter_parameter(response, 0x0005, 0);

    // Parameter 0x0006: Total uncorrected errors
    add_counter_parameter(response, 0x0006, 0);
}

/// Page 0x03: Read Error Counters
fn add_read_error_counters(response: &mut Vec<u8>) {
    // Same structure as write errors
    add_counter_parameter(response, 0x0000, 0);
    add_counter_parameter(response, 0x0001, 0);
    add_counter_parameter(response, 0x0002, 0);
    add_counter_parameter(response, 0x0003, 0);
    add_counter_parameter(response, 0x0004, 0);
    add_counter_parameter(response, 0x0005, 0);
    add_counter_parameter(response, 0x0006, 0);
}

/// Page 0x06: Non-Medium Error Counters
fn add_non_medium_error_counters(response: &mut Vec<u8>) {
    // Parameter 0x0000: Non-medium error count
    add_counter_parameter(response, 0x0000, 0);
}

/// Page 0x0D: Temperature
fn add_temperature_log(response: &mut Vec<u8>) {
    // Parameter 0x0000: Current temperature (in Celsius)
    // 25°C = 0x19
    add_log_parameter(response, 0x0000, &[0x00, 0x00, 0x00, 0x19]);

    // Parameter 0x0001: Reference temperature
    add_log_parameter(response, 0x0001, &[0x00, 0x00, 0x00, 0x19]);
}

/// Page 0x2E: TapeAlert log page
///
/// Defined in the TapeAlert spec (and adopted by SSC-2 §8.2.3). Reports up to
/// 64 boolean flags (1 = condition active, 0 = clear) with parameter codes
/// 0x0001..0x0040. We emit all 64 flags as 0 — a healthy virtual drive has
/// nothing to alert about. Backup software polls this page on every mount
/// and after every error.
fn add_tape_alert_log(response: &mut Vec<u8>) {
    for flag in 1u16..=64u16 {
        // One-byte payload per flag, value 0 = no alert.
        add_log_parameter(response, flag, &[0u8]);
    }
}

/// Page 0x11: DT Device Status (SSC-4 §8.2.3)
///
/// sg_logs (sg3_utils) decodes parameter 0x0000 as "Very High Frequency Data"
/// and expects an 8-byte payload, so the parameter total must be at least
/// 12 bytes (4-byte header + 8-byte data). The previous 4-byte counter form
/// was rejected as "parameter length >= 12 expected, got 8".
fn add_dt_device_status_log(response: &mut Vec<u8>) {
    // Parameter 0x0000: Very High Frequency Data (8 bytes)
    // Bit layout (byte 0): bit 7=PAMR, bit 6=BPEW, bit 5=BIS, bit 4=MEDIUM_PRESENT,
    //                      bit 3=DT_DEV_OPEN, bit 2=READ_ONLY_FORMAT, bits 1-0=reserved.
    // We report MEDIUM_PRESENT=0, no special flags. Bytes 1-7 are reserved.
    add_log_parameter(response, 0x0000, &[0u8; 8]);

    // Parameter 0x0001: Very High Frequency Polling Delay (2 bytes, reserved=0)
    add_log_parameter(response, 0x0001, &[0u8, 0u8]);

    // Parameter 0x0002: DT Device ADT Data Encryption Control Status (12 bytes, reserved=0)
    add_log_parameter(response, 0x0002, &[0u8; 12]);

    // Parameter 0x0003: Key Management Error Data (12 bytes, reserved=0)
    add_log_parameter(response, 0x0003, &[0u8; 12]);
}

/// Page 0x12: Tape Alert Response (SSC-3 §8.2.4)
///
/// The host-poll-side companion to the 0x2E TapeAlert page. Per SSC, the
/// page reports the sequence number of the most-recently-raised TapeAlert
/// event and which flags are currently set. A drive that has never raised
/// any TapeAlert event returns either an empty parameter list (header
/// only) or a single all-zero parameter — both are spec-legal and convey
/// "no alert history".
///
/// thurvtl is a virtual drive with no fault model. We follow the same
/// "header-only / no parameters" shape used by 0x16 (Last n Error Events):
/// honest answer is "no events recorded".
fn add_tape_alert_response_log(_response: &mut Vec<u8>) {
    // No parameters. The 4-byte page header is appended by `handle_log_sense`.
}

/// Page 0x14: Device Statistics (SSC-5 §8.5)
///
/// Lifetime drive counters polled by backup software for health reporting.
/// thurvtl is a virtual drive — every counter reads zero. The parameter
/// list and per-parameter widths follow SSC-5 so sg_logs and TapeAlert-aware
/// monitoring decode the page cleanly.
fn add_device_statistics_log(response: &mut Vec<u8>, mfg_serial: &str) {
    // 0x0000 Lifetime Volume Loads — 8-byte counter
    add_log_parameter(response, 0x0000, &[0u8; 8]);
    // 0x0001 Lifetime Cleaning Operations — 8-byte counter
    add_log_parameter(response, 0x0001, &[0u8; 8]);
    // 0x0002 Lifetime Power On Hours — 4-byte counter
    add_log_parameter(response, 0x0002, &[0u8; 4]);
    // 0x0003 Lifetime Medium Motion Hours — 4-byte counter
    add_log_parameter(response, 0x0003, &[0u8; 4]);
    // 0x0004 Lifetime Metres of Tape Processed — 8-byte counter
    add_log_parameter(response, 0x0004, &[0u8; 8]);
    // 0x0005 Lifetime Medium Motion Hours since last Incompatible Volume Load
    add_log_parameter(response, 0x0005, &[0u8; 4]);
    // 0x0006 Lifetime Power On Hours since last Temperature Recalibration
    add_log_parameter(response, 0x0006, &[0u8; 4]);
    // 0x0007 Lifetime Power On Hours since last Power Cycle of the Device
    add_log_parameter(response, 0x0007, &[0u8; 4]);
    // 0x0040 Drive Manufacturer's Serial Number — variable ASCII. Same string
    // exposed by INQUIRY VPD page 0xB1, so a host that correlates the two
    // sources sees one identity per drive LUN.
    add_log_parameter(response, 0x0040, mfg_serial.as_bytes());
}

/// Page 0x16: Last n Error Events (SSC-5 §8.6)
///
/// Most-recent-first list of vendor-specific error events. A drive with a
/// clean error history reports an empty parameter list (header only) — the
/// emulated drive doesn't fault, so that's what we report.
fn add_last_n_error_events_log(_response: &mut Vec<u8>) {
    // No parameters. The 4-byte page header is appended by `handle_log_sense`.
}

/// Page 0x0C: Sequential Access Device (SSC-5 §8.5)
///
/// Cumulative bytes-transferred counters and partition-capacity hints.
/// Supersedes the deprecated 0x30 Tape Usage page. thurvtl is a virtual
/// drive and doesn't track per-LUN byte counters or model partition
/// capacity yet, so every parameter reads zero. The parameter list shape
/// follows SSC-5 so sg_logs decodes the page cleanly.
fn add_sequential_access_device_log(response: &mut Vec<u8>) {
    // 0x0000 Data Bytes Received From Initiator — 8-byte counter
    add_log_parameter(response, 0x0000, &[0u8; 8]);
    // 0x0001 Data Bytes Written To Media — 8-byte counter
    add_log_parameter(response, 0x0001, &[0u8; 8]);
    // 0x0002 Data Bytes Read From Media — 8-byte counter
    add_log_parameter(response, 0x0002, &[0u8; 8]);
    // 0x0003 Data Bytes Transferred To Initiator — 8-byte counter
    add_log_parameter(response, 0x0003, &[0u8; 8]);
    // 0x0004 Native Capacity From BOP to Current Position (MB) — 8-byte
    add_log_parameter(response, 0x0004, &[0u8; 8]);
    // 0x0005 Native Capacity Between Current Position and EOD (MB) — 8-byte
    add_log_parameter(response, 0x0005, &[0u8; 8]);
    // 0x0006 Native Capacity from BOP to EOD of partition (MB) — 8-byte
    add_log_parameter(response, 0x0006, &[0u8; 8]);
    // 0x0007 Native Capacity of partition (MB) — 8-byte
    add_log_parameter(response, 0x0007, &[0u8; 8]);
    // 0x0008 Cleaning Required — 1 byte (bit 0 = needs cleaning)
    add_log_parameter(response, 0x0008, &[0u8]);
}

/// Page 0x1B: Data Compression (SSC-5; replaces the deprecated 0x32)
///
/// Read / Write compression ratios and cumulative byte counters. Ratios
/// are reported as fixed-point values where `0x0100` = 1.00. thurvtl
/// reports 1:1 because the SCSI surface doesn't observe drive compression
/// as a payload-size change — block payloads are decompressed before
/// LOG SENSE / MODE SENSE see them. All byte counters zero on the virtual
/// drive.
fn add_data_compression_log(response: &mut Vec<u8>) {
    // 0x0000 Read Compression Ratio (×100) — 2 bytes. 0x0100 = 1.00.
    add_log_parameter(response, 0x0000, &[0x01, 0x00]);
    // 0x0001 Write Compression Ratio (×100) — 2 bytes
    add_log_parameter(response, 0x0001, &[0x01, 0x00]);
    // 0x0002 Megabytes Transferred to Server — 4 bytes
    add_log_parameter(response, 0x0002, &[0u8; 4]);
    // 0x0003 Bytes Transferred to Server — 4 bytes
    add_log_parameter(response, 0x0003, &[0u8; 4]);
    // 0x0004 Megabytes Read From Tape — 4 bytes
    add_log_parameter(response, 0x0004, &[0u8; 4]);
    // 0x0005 Bytes Read From Tape — 4 bytes
    add_log_parameter(response, 0x0005, &[0u8; 4]);
    // 0x0006 Megabytes Transferred From Server — 4 bytes
    add_log_parameter(response, 0x0006, &[0u8; 4]);
    // 0x0007 Bytes Transferred From Server — 4 bytes
    add_log_parameter(response, 0x0007, &[0u8; 4]);
    // 0x0008 Megabytes Written To Tape — 4 bytes
    add_log_parameter(response, 0x0008, &[0u8; 4]);
    // 0x0009 Bytes Written To Tape — 4 bytes
    add_log_parameter(response, 0x0009, &[0u8; 4]);
}

/// Page 0x1A: Power Condition Transitions (SPC-4 §7.3.16)
///
/// Cumulative count of transitions into each power state. Companion to
/// MODE page 0x1A (Power Condition) which sets the timers. Hosts use
/// the page to confirm the drive is honoring power-condition policy.
///
/// thurvtl is a virtual drive — power state is whatever the host
/// kernel does, and we never transition on our own. Every counter is
/// zero. Six 4-byte parameters cover the SPC-4-defined transitions:
/// Active, Idle_a, Idle_b, Idle_c, Standby_y, Standby_z.
fn add_power_condition_transitions_log(response: &mut Vec<u8>) {
    // 0x0001 Accumulated Transitions to Active — 4-byte counter
    add_log_parameter(response, 0x0001, &[0u8; 4]);
    // 0x0002 Accumulated Transitions to Idle_a — 4-byte counter
    add_log_parameter(response, 0x0002, &[0u8; 4]);
    // 0x0003 Accumulated Transitions to Idle_b — 4-byte counter
    add_log_parameter(response, 0x0003, &[0u8; 4]);
    // 0x0004 Accumulated Transitions to Idle_c — 4-byte counter
    add_log_parameter(response, 0x0004, &[0u8; 4]);
    // 0x0005 Accumulated Transitions to Standby_y — 4-byte counter
    add_log_parameter(response, 0x0005, &[0u8; 4]);
    // 0x0006 Accumulated Transitions to Standby_z — 4-byte counter
    add_log_parameter(response, 0x0006, &[0u8; 4]);
}

/// Page 0x17: Volume Statistics (SSC-5 §8.7)
///
/// Per-mounted-volume counters. With no volume mounted (or on a virtual drive
/// that doesn't track these), Validity=0 with all-zero counters is the
/// correct shape.
fn add_volume_statistics_log(response: &mut Vec<u8>) {
    // 0x0000 Validity flag — 1 byte (0 = parameters not valid)
    add_log_parameter(response, 0x0000, &[0u8]);
    // 0x0001 Volume Mounts — 8-byte counter
    add_log_parameter(response, 0x0001, &[0u8; 8]);
    // 0x0002 Volume Recovered Write Data Errors — 8-byte counter
    add_log_parameter(response, 0x0002, &[0u8; 8]);
    // 0x0003 Volume Unrecovered Write Data Errors — 8-byte counter
    add_log_parameter(response, 0x0003, &[0u8; 8]);
    // 0x0004 Volume Recovered Read Data Errors — 8-byte counter
    add_log_parameter(response, 0x0004, &[0u8; 8]);
    // 0x0005 Volume Unrecovered Read Data Errors — 8-byte counter
    add_log_parameter(response, 0x0005, &[0u8; 8]);
    // 0x0006 Volume Mounts Beyond Lifetime Specification — 8-byte counter
    add_log_parameter(response, 0x0006, &[0u8; 8]);
    // 0x0007 Volume Recovered Write Errors Beyond Specification — 8-byte
    add_log_parameter(response, 0x0007, &[0u8; 8]);
}

/// Page 0x30: Tape Usage (legacy; deprecated by SSC-5)
///
/// Pre-SSC-3 page tracking lifetime drive activity. Modern equivalents
/// are 0x14 Device Statistics + 0x0C Sequential Access Device. Legacy
/// backup software (older NetBackup, Bareos, some pre-LTO-7 management
/// tools) still polls this page during drive-capability detection.
///
/// Parameter codes + widths follow the SSC legacy 0x30/0x31/0x32
/// family. thurvtl is a virtual drive — every
/// counter zero.
fn add_tape_usage_legacy_log(response: &mut Vec<u8>) {
    // 0x0001 Thread count (loads) — 8-byte counter
    add_log_parameter(response, 0x0001, &[0u8; 8]);
    // 0x0002 Total data sets written — 8-byte counter
    add_log_parameter(response, 0x0002, &[0u8; 8]);
    // 0x0003 Total write retries — 4-byte counter
    add_log_parameter(response, 0x0003, &[0u8; 4]);
    // 0x0004 Total unrecovered write errors — 2-byte counter
    add_log_parameter(response, 0x0004, &[0u8; 2]);
    // 0x0005 Total suspended writes — 2-byte counter
    add_log_parameter(response, 0x0005, &[0u8; 2]);
    // 0x0006 Total fatal suspended writes — 2-byte counter
    add_log_parameter(response, 0x0006, &[0u8; 2]);
    // 0x0007 Total data sets read — 8-byte counter
    add_log_parameter(response, 0x0007, &[0u8; 8]);
    // 0x0008 Total read retries — 4-byte counter
    add_log_parameter(response, 0x0008, &[0u8; 4]);
    // 0x0009 Total unrecovered read errors — 2-byte counter
    add_log_parameter(response, 0x0009, &[0u8; 2]);
    // 0x000A Total suspended reads — 2-byte counter
    add_log_parameter(response, 0x000A, &[0u8; 2]);
    // 0x000B Total fatal suspended reads — 2-byte counter
    add_log_parameter(response, 0x000B, &[0u8; 2]);
}

/// Page 0x31: Tape Capacity (legacy)
///
/// Per-partition remaining / maximum capacity in MiB. Legacy companion
/// to 0x0C Sequential Access Device. thurvtl doesn't model partition
/// capacity yet — every counter zero (the same shape 0x0C reports).
fn add_tape_capacity_legacy_log(response: &mut Vec<u8>) {
    // 0x0001 Main partition remaining capacity — 4-byte MB
    add_log_parameter(response, 0x0001, &[0u8; 4]);
    // 0x0002 Alternate partition remaining capacity — 4-byte MB
    add_log_parameter(response, 0x0002, &[0u8; 4]);
    // 0x0003 Main partition maximum capacity — 4-byte MB
    add_log_parameter(response, 0x0003, &[0u8; 4]);
    // 0x0004 Alternate partition maximum capacity — 4-byte MB
    add_log_parameter(response, 0x0004, &[0u8; 4]);
}

/// Page 0x32: Data Compression (legacy; deprecated by SSC-5)
///
/// Same per-counter intent as 0x1B but with the older parameter codes
/// pre-LTO-7 backup software keys on. 1:1 ratio (0x0100) reflects the
/// same fact: the SCSI surface doesn't observe drive compression as a
/// payload-size change. All byte counters zero on the virtual drive.
fn add_data_compression_legacy_log(response: &mut Vec<u8>) {
    // 0x0000 Read compression ratio (×100) — 2 bytes, 0x0100 = 1.00
    add_log_parameter(response, 0x0000, &[0x01, 0x00]);
    // 0x0001 Write compression ratio (×100) — 2 bytes
    add_log_parameter(response, 0x0001, &[0x01, 0x00]);
    // 0x0002 MB transferred to server — 4 bytes
    add_log_parameter(response, 0x0002, &[0u8; 4]);
    // 0x0003 Bytes transferred to server — 4 bytes
    add_log_parameter(response, 0x0003, &[0u8; 4]);
    // 0x0004 MB read from tape — 4 bytes
    add_log_parameter(response, 0x0004, &[0u8; 4]);
    // 0x0005 Bytes read from tape — 4 bytes
    add_log_parameter(response, 0x0005, &[0u8; 4]);
    // 0x0006 MB transferred from server — 4 bytes
    add_log_parameter(response, 0x0006, &[0u8; 4]);
    // 0x0007 Bytes transferred from server — 4 bytes
    add_log_parameter(response, 0x0007, &[0u8; 4]);
    // 0x0008 MB written to tape — 4 bytes
    add_log_parameter(response, 0x0008, &[0u8; 4]);
    // 0x0009 Bytes written to tape — 4 bytes
    add_log_parameter(response, 0x0009, &[0u8; 4]);
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Add a log parameter to the response
fn add_log_parameter(response: &mut Vec<u8>, param_code: u16, data: &[u8]) {
    response.extend_from_slice(&param_code.to_be_bytes()); // Parameter code (2 bytes)
    response.push(0x00); // Control byte (DU=0, DS=0, TSD=0, ETC=0, TMC=0, LBIN=0, LP=0)
    response.push(data.len() as u8); // Parameter length
    response.extend_from_slice(data); // Parameter data
}

/// Add a counter parameter (4-byte or 8-byte counter)
fn add_counter_parameter(response: &mut Vec<u8>, param_code: u16, value: u64) {
    // Use 4-byte counter for MVP (sufficient for counters)
    let value_bytes = (value as u32).to_be_bytes();
    add_log_parameter(response, param_code, &value_bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_sense_supported_pages() {
        let result = handle_log_sense(0x00, 0x00, 0x01, "THUR-MFG-001");
        assert!(result.is_ok());
        let data = result.unwrap();

        // Should have header + multiple parameters
        assert!(data.len() > 4);
        assert_eq!(data[0], 0x00); // Page code
        assert_eq!(data[1], 0x00); // Subpage
    }

    #[test]
    fn test_log_sense_write_errors() {
        let result = handle_log_sense(0x02, 0x00, 0x01, "THUR-MFG-001");
        assert!(result.is_ok());
        let data = result.unwrap();

        // Should have header + 7 parameters (each 8 bytes: 2 code + 1 control + 1 len + 4 data)
        assert_eq!(data.len(), 4 + (7 * 8));
        assert_eq!(data[0], 0x02); // Page code
    }

    #[test]
    fn test_log_sense_read_errors() {
        let result = handle_log_sense(0x03, 0x00, 0x01, "THUR-MFG-001");
        assert!(result.is_ok());
        let data = result.unwrap();

        assert_eq!(data.len(), 4 + (7 * 8));
        assert_eq!(data[0], 0x03); // Page code
    }

    #[test]
    fn test_log_sense_temperature() {
        let result = handle_log_sense(0x0D, 0x00, 0x01, "THUR-MFG-001");
        assert!(result.is_ok());
        let data = result.unwrap();

        // Header + 2 temperature parameters
        assert!(data.len() > 10);
        assert_eq!(data[0], 0x0D); // Page code
    }

    #[test]
    fn test_log_sense_unsupported() {
        let result = handle_log_sense(0xFF, 0x00, 0x01, "THUR-MFG-001");
        assert!(result.is_err());
    }

    #[test]
    fn test_log_sense_device_statistics() {
        let result = handle_log_sense(0x14, 0x00, 0x01, "THUR-MFG-001").unwrap();
        assert_eq!(result[0], 0x14);
        assert_eq!(result[1], 0x00);
        // 4-byte page header + parameter list
        assert!(result.len() > 4);
        let page_len = u16::from_be_bytes([result[2], result[3]]) as usize;
        assert_eq!(page_len, result.len() - 4);
        // First parameter: code 0x0000, 8-byte payload (12 bytes total).
        assert_eq!(&result[4..6], &[0x00, 0x00]);
        assert_eq!(result[7], 8);
    }

    #[test]
    fn test_log_sense_last_n_error_events() {
        let result = handle_log_sense(0x16, 0x00, 0x01, "THUR-MFG-001").unwrap();
        // Header-only response; no parameters.
        assert_eq!(result.len(), 4);
        assert_eq!(result[0], 0x16);
        assert_eq!(u16::from_be_bytes([result[2], result[3]]), 0);
    }

    #[test]
    fn test_log_sense_volume_statistics() {
        let result = handle_log_sense(0x17, 0x00, 0x01, "THUR-MFG-001").unwrap();
        assert_eq!(result[0], 0x17);
        let page_len = u16::from_be_bytes([result[2], result[3]]) as usize;
        assert_eq!(page_len, result.len() - 4);
        // First parameter: validity flag, 1 byte.
        assert_eq!(&result[4..6], &[0x00, 0x00]);
        assert_eq!(result[7], 1);
    }

    #[test]
    fn test_log_sense_supported_pages_lists_new_pages() {
        let data = handle_log_sense(0x00, 0x00, 0x01, "THUR-MFG-001").unwrap();
        // Each entry is a 5-byte log parameter (4-byte header + 1-byte data).
        // Walk the list and check 0x0C / 0x12 / 0x14 / 0x16 / 0x17 / 0x1A / 0x1B appear.
        let mut found = [false; 7];
        let mut i = 4;
        while i + 5 <= data.len() {
            let code = data[i + 4];
            match code {
                0x0C => found[0] = true,
                0x12 => found[1] = true,
                0x14 => found[2] = true,
                0x16 => found[3] = true,
                0x17 => found[4] = true,
                0x1A => found[5] = true,
                0x1B => found[6] = true,
                _ => {}
            }
            i += 5;
        }
        assert!(
            found.iter().all(|f| *f),
            "0x0C/0x12/0x14/0x16/0x17/0x1A/0x1B not all listed"
        );
    }

    #[test]
    fn test_log_sense_tape_usage_legacy() {
        let result = handle_log_sense(0x30, 0x00, 0x01, "THUR-MFG-001").unwrap();
        assert_eq!(result[0], 0x30);
        let page_len = u16::from_be_bytes([result[2], result[3]]) as usize;
        assert_eq!(page_len, result.len() - 4);
        // First parameter: code 0x0001, 8-byte payload (12 bytes total).
        assert_eq!(&result[4..6], &[0x00, 0x01]);
        assert_eq!(result[7], 8);
    }

    #[test]
    fn test_log_sense_tape_capacity_legacy() {
        let result = handle_log_sense(0x31, 0x00, 0x01, "THUR-MFG-001").unwrap();
        assert_eq!(result[0], 0x31);
        let page_len = u16::from_be_bytes([result[2], result[3]]) as usize;
        assert_eq!(page_len, result.len() - 4);
        // Four 4-byte parameters → 4 × (4 header + 4 data) = 32 bytes body.
        assert_eq!(result.len(), 4 + 32);
    }

    #[test]
    fn test_log_sense_data_compression_legacy() {
        let result = handle_log_sense(0x32, 0x00, 0x01, "THUR-MFG-001").unwrap();
        assert_eq!(result[0], 0x32);
        let page_len = u16::from_be_bytes([result[2], result[3]]) as usize;
        assert_eq!(page_len, result.len() - 4);
        // First parameter: read compression ratio, 2-byte payload, 0x0100.
        assert_eq!(result[7], 2);
        assert_eq!(&result[8..10], &[0x01, 0x00]);
    }

    #[test]
    fn test_log_sense_power_condition_transitions() {
        let result = handle_log_sense(0x1A, 0x00, 0x01, "THUR-MFG-001").unwrap();
        assert_eq!(result[0], 0x1A);
        let page_len = u16::from_be_bytes([result[2], result[3]]) as usize;
        assert_eq!(page_len, result.len() - 4);
        // Six 4-byte parameters → 6 × (4 header + 4 data) = 48 bytes body.
        assert_eq!(result.len(), 4 + 48);
        // First parameter: code 0x0001, 4-byte payload.
        assert_eq!(&result[4..6], &[0x00, 0x01]);
        assert_eq!(result[7], 4);
    }

    #[test]
    fn test_log_sense_tape_alert_response() {
        let result = handle_log_sense(0x12, 0x00, 0x01, "THUR-MFG-001").unwrap();
        // Header-only response; no parameters. Mirrors the 0x16 shape — "no
        // events recorded" is the truthful answer for a virtual drive.
        assert_eq!(result.len(), 4);
        assert_eq!(result[0], 0x12);
        assert_eq!(u16::from_be_bytes([result[2], result[3]]), 0);
    }

    #[test]
    fn test_log_sense_sequential_access_device() {
        let result = handle_log_sense(0x0C, 0x00, 0x01, "THUR-MFG-001").unwrap();
        assert_eq!(result[0], 0x0C);
        assert_eq!(result[1], 0x00);
        let page_len = u16::from_be_bytes([result[2], result[3]]) as usize;
        assert_eq!(page_len, result.len() - 4);
        // First parameter: code 0x0000, 8-byte payload (12 bytes total).
        assert_eq!(&result[4..6], &[0x00, 0x00]);
        assert_eq!(result[7], 8);
    }

    #[test]
    fn test_log_sense_data_compression() {
        let result = handle_log_sense(0x1B, 0x00, 0x01, "THUR-MFG-001").unwrap();
        assert_eq!(result[0], 0x1B);
        let page_len = u16::from_be_bytes([result[2], result[3]]) as usize;
        assert_eq!(page_len, result.len() - 4);
        // First parameter: read compression ratio, 2-byte payload, value 0x0100.
        assert_eq!(&result[4..6], &[0x00, 0x00]); // param code
        assert_eq!(result[7], 2); // length
        assert_eq!(&result[8..10], &[0x01, 0x00]); // 1:1 ratio
    }
}
