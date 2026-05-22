// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! SCSI sense data — keys, ASC/ASCQ table, and the unified
//! [`SenseData`] / [`SenseDataBuilder`] types that produce
//! either fixed-format (response code 0x70, 18 bytes) or
//! descriptor-format (0x72, 8+ bytes) wire bytes.
//!
//! Tape callers (thurvtld) reach for `SenseDataBuilder` —
//! fluent fixed-format API with the SSC-4 Filemark / EOM / ILI flag
//! bits. Block callers (thurvsad) reach for
//! `SenseData::new(key, asc, ascq)` and serialize via
//! [`SenseData::to_descriptor_bytes`] for the 8-byte short form.
//! Both APIs build the same underlying value, so a future
//! handler can pick whichever is ergonomically clearer.
//!
//! The product-specific `<DomainError> -> SenseData` walkers
//! stay in their owning crate (thurvtld's `error_to_sense`
//! pattern-matches `SmcError`; thurvsa's per-arm helpers live
//! alongside the SBC-3 dispatcher) — those are too tied to the
//! product's error enum to belong here.
//!
//! Some constants are unused on a given product but kept here
//! so the spec table stays in one place.

#![allow(dead_code)]

use tracing::info;

/// SCSI Sense Keys (SPC-3 Table 27)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SenseKey {
    NoSense = 0x00,
    RecoveredError = 0x01,
    NotReady = 0x02,
    MediumError = 0x03,
    HardwareError = 0x04,
    IllegalRequest = 0x05,
    UnitAttention = 0x06,
    DataProtect = 0x07,
    BlankCheck = 0x08,
    VendorSpecific = 0x09,
    CopyAborted = 0x0A,
    AbortedCommand = 0x0B,
    Equal = 0x0C,
    VolumeOverflow = 0x0D,
    Miscompare = 0x0E,
}

/// Additional Sense Code (ASC/ASCQ) combinations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdditionalSenseCode {
    pub asc: u8,
    pub ascq: u8,
}

// Common ASC/ASCQ codes for tape (SSC-2 Table 165)
pub const ASC_NO_ADDITIONAL_INFO: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x00,
    ascq: 0x00,
};
pub const ASC_FILEMARK_DETECTED: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x00,
    ascq: 0x01,
};
pub const ASC_EOM_DETECTED: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x00,
    ascq: 0x02,
};
pub const ASC_SETMARK_DETECTED: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x00,
    ascq: 0x03,
};
pub const ASC_BOT_DETECTED: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x00,
    ascq: 0x04,
};
pub const ASC_EOD_DETECTED: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x00,
    ascq: 0x05,
};
pub const ASC_NOT_READY_BECOMING_READY: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x04,
    ascq: 0x01,
};
/// "LOGICAL UNIT NOT READY, OPERATION IN PROGRESS." Surfaced when the
/// daemon's upload backpressure gate timed out — the local pool is at
/// its hard cap (or the disk-free floor was hit) and uploads couldn't
/// drain the backlog within `upload.backpressure_max_wait_seconds`.
/// Backup software (tar/mt, NetBackup, Veeam, Bacula) treats this as
/// transient and retries.
pub const ASC_NOT_READY_OPERATION_IN_PROGRESS: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x04,
    ascq: 0x07,
};
pub const ASC_NOT_READY_NO_MEDIUM: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x3A,
    ascq: 0x00,
};
pub const ASC_MEDIUM_CHANGED: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x28,
    ascq: 0x00,
};
pub const ASC_POWER_ON_RESET: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x29,
    ascq: 0x00,
};
pub const ASC_WRITE_PROTECTED: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x27,
    ascq: 0x00,
};
/// Data Protect / "WRITE PROTECTED — WORM MEDIUM" (SSC-5 § ASC table).
/// Used when a WORM cartridge refuses WRITE / WRITE FILEMARKS at a
/// non-EOD LBA, or refuses ERASE / FORMAT MEDIUM / ALLOW OVERWRITE.
pub const ASC_WRITE_PROTECTED_WORM: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x30,
    ascq: 0x0C,
};
/// Data Protect / "CONDITIONAL WRITE PROTECT" (SPC-4 § ASC table).
/// Used when LTO-7+ Append-Only mode (Mode Page 0x10/0x01 WRITE MODE
/// = 1) refuses a WRITE / WRITE FILEMARKS at a non-EOD position.
/// "Conditional" because flipping append-only off restores normal
/// write semantics — distinct from WORM's sticky 0x30/0x0C.
pub const ASC_CONDITIONAL_WRITE_PROTECT: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x27,
    ascq: 0x06,
};
/// Data Protect / "ENCRYPTION KEY ABSENT" (SPC-4 § ASC table 74h
/// codes). Used when LTO-8+ Encrypt-Only mode (Mode Page 0x10/0x01
/// WRE bit) refuses a WRITE / WRITE FILEMARKS because no drive
/// encryption key has been installed via SECURITY PROTOCOL OUT.
pub const ASC_ENCRYPTION_KEY_ABSENT: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x74,
    ascq: 0x0C,
};
pub const ASC_INVALID_FIELD_IN_CDB: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x24,
    ascq: 0x00,
};
/// Illegal Request / "INVALID FIELD IN PARAMETER LIST" (SPC-4 §
/// ASC table). Distinct from INVALID FIELD IN CDB which guards
/// CDB bytes — this guards the inbound parameter list payload
/// (MODE SELECT, PERSISTENT RESERVE OUT, UNMAP, …).
pub const ASC_INVALID_FIELD_IN_PARAMETER_LIST: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x26,
    ascq: 0x00,
};
/// Illegal Request / "INCOMPATIBLE MEDIUM INSTALLED" (SPC-4 § ASC
/// table). Returned when a cartridge cannot be loaded into the target
/// drive because the cartridge generation is newer than the drive
/// supports — including LTO-7 Type M (M8 barcode) on an LTO-7 drive.
pub const ASC_INCOMPATIBLE_MEDIUM_INSTALLED: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x30,
    ascq: 0x00,
};
/// Aborted Command / "LOGICAL BLOCK GUARD CHECK FAILED" (SPC-4 § ASC
/// table 10h). Returned on READ when a freshly-computed CRC32C
/// (Logical Block Protection) does not match a host-supplied trailer
/// — currently unreachable on Thur VTL because LBP CRCs are
/// computed-on-the-fly from BLAKE3-verified data, but kept defined so
/// the constant is stable if a future check path needs it.
pub const ASC_LOGICAL_BLOCK_GUARD_CHECK_FAILED: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x10,
    ascq: 0x01,
};
/// Aborted Command / "LOGICAL BLOCK PROTECTION METHOD ERROR" (SPC-4 §
/// ASC table 10h). Returned on WRITE when the host sets WRPROTECT > 0
/// but the trailing 4-byte CRC32C either is shorter than the trailer
/// width or fails to validate.
pub const ASC_LOGICAL_BLOCK_PROTECTION_METHOD_ERROR: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x10,
    ascq: 0x05,
};
/// Illegal Request / "MEDIUM REMOVAL PREVENTED" (SPC-4 § ASC table).
/// Returned when SCSI UNLOAD or MOVE MEDIUM-from-drive is rejected
/// because an initiator has asserted PREVENT/ALLOW MEDIUM REMOVAL
/// (cdb[4] bit 0 = data transport).
pub const ASC_MEDIUM_REMOVAL_PREVENTED: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x53,
    ascq: 0x02,
};
pub const ASC_INVALID_COMMAND: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x20,
    ascq: 0x00,
};
/// Illegal Request / "LOGICAL UNIT NOT SUPPORTED" (SPC-4 § ASC
/// table). Emitted when the addressed LUN is not in the registry
/// for opcodes that don't have a per-LUN no-LUN special encoding
/// (READ CAPACITY, MODE SENSE, …; INQUIRY uses the SAM-5 "no
/// LUN" pattern instead).
pub const ASC_LU_NOT_SUPPORTED: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x25,
    ascq: 0x00,
};
pub const ASC_LBA_OUT_OF_RANGE: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x21,
    ascq: 0x00,
};
pub const ASC_UNRECOVERED_READ_ERROR: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x11,
    ascq: 0x00,
};
pub const ASC_WRITE_ERROR: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x0C,
    ascq: 0x00,
};
/// Miscompare / "MISCOMPARE DURING VERIFY OPERATION" (SBC-3 §5.2).
/// Emitted by COMPARE AND WRITE (0x89) when the on-disk bytes at
/// the requested LBA range disagree with the host's compare buffer;
/// the write half is suppressed.
pub const ASC_MISCOMPARE_DURING_VERIFY: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x1D,
    ascq: 0x00,
};
/// Illegal Request / "SAVING PARAMETERS NOT SUPPORTED" (SPC-4
/// §6.11/§6.13). MODE SELECT 6/10 with SP=1 — the device cannot
/// honour persisted mode pages.
pub const ASC_SAVING_PARAMETERS_NOT_SUPPORTED: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x39,
    ascq: 0x00,
};
/// Data Protect / "Logical block protection / decryption integrity check failed"
/// Used for AES-GCM authentication failure or when reading an encrypted block
/// without the correct drive key (SSC-4 §4.2.20).
pub const ASC_DATA_DECRYPTION_ERROR: AdditionalSenseCode = AdditionalSenseCode {
    asc: 0x74,
    ascq: 0x0C,
};

/// SCSI sense response code — fixed-format (0x70) carries the full
/// 18-byte block including INFORMATION / COMMAND-SPECIFIC fields;
/// descriptor-format (0x72) is an 8-byte header plus optional
/// descriptors. Real LTO drives return fixed-format on most error
/// paths; SBC-3 / SAM-5 modern devices can return either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenseFormat {
    /// Response code 0x70. Carries optional Information /
    /// Command-Specific / Filemark / EOM / ILI metadata. Default
    /// for tape callers.
    Fixed,
    /// Response code 0x72. Eight-byte header + zero or more
    /// descriptors (we don't append any). Default for thurvsa's
    /// short-form SBC-3 check conditions.
    Descriptor,
}

impl SenseFormat {
    /// Wire response-code byte for this format.
    pub const fn response_code(self) -> u8 {
        match self {
            SenseFormat::Fixed => 0x70,
            SenseFormat::Descriptor => 0x72,
        }
    }
}

/// Structured sense-data value. Carries the sense-key / ASC / ASCQ
/// triple plus the optional fixed-format flags (Information,
/// Command-Specific Information, Filemark, EOM, ILI).
///
/// Construct via [`SenseData::new`] (descriptor-format default) or
/// [`SenseData::fixed`] / [`SenseDataBuilder::new`] for the fluent
/// fixed-format API. Wire-format conversion is in
/// [`SenseData::to_bytes`] (uses `format`),
/// [`SenseData::to_fixed_bytes`] (forces 0x70), or
/// [`SenseData::to_descriptor_bytes`] (forces 0x72).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenseData {
    pub key: SenseKey,
    pub asc: u8,
    pub ascq: u8,
    pub format: SenseFormat,
    pub information: Option<u32>,
    pub command_specific_info: Option<u32>,
    pub ili: bool,
    pub eom: bool,
    pub filemark: bool,
}

impl SenseData {
    /// Construct a descriptor-format sense (response code 0x72) —
    /// the SBC-3 default. Block callers reach for this; tape
    /// callers usually go through [`SenseDataBuilder`].
    pub const fn new(key: SenseKey, asc: u8, ascq: u8) -> Self {
        Self {
            key,
            asc,
            ascq,
            format: SenseFormat::Descriptor,
            information: None,
            command_specific_info: None,
            ili: false,
            eom: false,
            filemark: false,
        }
    }

    /// Construct a fixed-format sense (response code 0x70) — the
    /// SSC-4 default for tape position / Filemark / EOM /
    /// VolumeOverflow paths.
    pub const fn fixed(key: SenseKey, asc: u8, ascq: u8) -> Self {
        Self {
            key,
            asc,
            ascq,
            format: SenseFormat::Fixed,
            information: None,
            command_specific_info: None,
            ili: false,
            eom: false,
            filemark: false,
        }
    }

    /// Construct from an [`AdditionalSenseCode`] pair. Defaults to
    /// fixed format — convenient for the SSC-4 surface where every
    /// ASC/ASCQ pair is listed as a named constant above.
    pub const fn from_asc(key: SenseKey, asc_code: AdditionalSenseCode) -> Self {
        Self::fixed(key, asc_code.asc, asc_code.ascq)
    }

    /// Set the INFORMATION field (e.g., LBA of error, difference in
    /// length on a short read). Fixed format only — descriptor
    /// callers ignore.
    pub fn with_information(mut self, info: u32) -> Self {
        self.information = Some(info);
        self
    }

    /// Set COMMAND-SPECIFIC INFORMATION. Fixed format only.
    pub fn with_command_info(mut self, info: u32) -> Self {
        self.command_specific_info = Some(info);
        self
    }

    /// Set the Incorrect Length Indicator (READ-side short reads).
    /// Fixed format only.
    pub fn with_ili(mut self) -> Self {
        self.ili = true;
        self
    }

    /// Set the End-Of-Medium flag. Fixed format only — paired with
    /// the SSC-4 EOM / Early-Warning / VolumeOverflow paths.
    pub fn with_eom(mut self) -> Self {
        self.eom = true;
        self
    }

    /// Set the Filemark-detected flag. Fixed format only — paired
    /// with the SSC-4 SPACE / READ filemark surface.
    pub fn with_filemark(mut self) -> Self {
        self.filemark = true;
        self
    }

    /// Encode using whichever format the value carries.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self.format {
            SenseFormat::Fixed => self.fixed_bytes_vec(),
            SenseFormat::Descriptor => self.to_descriptor_bytes().to_vec(),
        }
    }

    /// Force descriptor-format encoding (8 bytes, no descriptors).
    /// SBC-3 callers use this directly.
    pub fn to_descriptor_bytes(&self) -> [u8; 8] {
        [
            0x72,
            (self.key as u8) & 0x0F,
            self.asc,
            self.ascq,
            0,
            0,
            0,
            0,
        ]
    }

    /// Force fixed-format encoding (18 bytes). SSC-4 callers use
    /// this directly; SBC-3 callers usually want
    /// [`Self::to_descriptor_bytes`] instead.
    pub fn to_fixed_bytes(&self) -> [u8; 18] {
        let mut sense = [0u8; 18];

        sense[0] = 0x70;
        if self.information.is_some() {
            sense[0] |= 0x80; // Valid bit
        }

        sense[1] = 0x00;

        let mut byte2 = self.key as u8;
        if self.ili {
            byte2 |= 0x20;
        }
        if self.eom {
            byte2 |= 0x40;
        }
        if self.filemark {
            byte2 |= 0x80;
        }
        sense[2] = byte2;

        if let Some(info) = self.information {
            sense[3..7].copy_from_slice(&info.to_be_bytes());
        }

        sense[7] = 10;

        if let Some(cmd_info) = self.command_specific_info {
            sense[8..12].copy_from_slice(&cmd_info.to_be_bytes());
        }

        sense[12] = self.asc;
        sense[13] = self.ascq;
        sense[14] = 0x00;
        sense[15] = 0x00;
        sense[16] = 0x00;
        sense[17] = 0x00;

        sense
    }

    fn fixed_bytes_vec(self) -> Vec<u8> {
        self.to_fixed_bytes().to_vec()
    }

    /// Decode wire-format sense bytes back into a structured value.
    /// Recognizes both fixed (0x70) and descriptor (0x72) response
    /// codes. Returns `None` if the buffer is too short or the
    /// response code / sense key isn't one we model.
    ///
    /// thurvtl's `into_shared_response` adapter uses this to lift
    /// the legacy `Vec<u8>` sense-builder output back into the
    /// structured shape the canonical `ScsiResponse` carries.
    pub fn from_wire_bytes(bytes: &[u8]) -> Option<Self> {
        let response_code = *bytes.first()? & 0x7F;
        match response_code {
            0x70 | 0x71 => Self::from_fixed_bytes(bytes),
            0x72 | 0x73 => Self::from_descriptor_bytes(bytes),
            _ => None,
        }
    }

    fn from_fixed_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 14 {
            return None;
        }
        let valid = (bytes[0] & 0x80) != 0;
        let byte2 = bytes[2];
        let key = decode_sense_key(byte2 & 0x0F)?;
        let mut sd = Self::fixed(key, bytes[12], bytes[13]);
        if valid {
            let info = u32::from_be_bytes([bytes[3], bytes[4], bytes[5], bytes[6]]);
            sd = sd.with_information(info);
        }
        if (byte2 & 0x80) != 0 {
            sd = sd.with_filemark();
        }
        if (byte2 & 0x40) != 0 {
            sd = sd.with_eom();
        }
        if (byte2 & 0x20) != 0 {
            sd = sd.with_ili();
        }
        Some(sd)
    }

    fn from_descriptor_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 4 {
            return None;
        }
        let key = decode_sense_key(bytes[1] & 0x0F)?;
        Some(Self::new(key, bytes[2], bytes[3]))
    }

    // ===== SPC-4 / SBC-3 named sense constants =====
    //
    // Shorthand for the most-frequent (key, ASC, ASCQ) tuples
    // products emit, so call sites read like
    // `ScsiResponse::check(SenseData::INVALID_OPCODE)` instead of
    // hand-rolling the triple. Format defaults to descriptor (0x72)
    // since SBC-3 callers consume these the most; tape callers
    // wanting fixed format reach for [`SenseDataBuilder`].

    /// ASC/ASCQ 0x20/0x00 — INVALID COMMAND OPERATION CODE.
    pub const INVALID_OPCODE: Self = Self::new(SenseKey::IllegalRequest, 0x20, 0x00);

    /// ASC/ASCQ 0x24/0x00 — INVALID FIELD IN CDB.
    pub const INVALID_FIELD_IN_CDB: Self = Self::new(SenseKey::IllegalRequest, 0x24, 0x00);

    /// ASC/ASCQ 0x25/0x00 — LOGICAL UNIT NOT SUPPORTED.
    pub const LU_NOT_SUPPORTED: Self = Self::new(SenseKey::IllegalRequest, 0x25, 0x00);

    /// ASC/ASCQ 0x04/0x00 — LOGICAL UNIT NOT READY, CAUSE NOT
    /// REPORTABLE.
    pub const LU_NOT_READY: Self = Self::new(SenseKey::NotReady, 0x04, 0x00);

    /// ASC/ASCQ 0x04/0x07 — LOGICAL UNIT NOT READY, OPERATION IN
    /// PROGRESS. Surfaced when chunk-seal is parking on the
    /// per-backend `PoolBudget` (upload backpressure) and the wait
    /// deadline fires. Backup software treats this as transient and
    /// retries.
    pub const LU_NOT_READY_OPERATION_IN_PROGRESS: Self = Self::new(SenseKey::NotReady, 0x04, 0x07);

    /// ASC/ASCQ 0x21/0x00 — LOGICAL BLOCK ADDRESS OUT OF RANGE.
    pub const LBA_OUT_OF_RANGE: Self = Self::new(SenseKey::IllegalRequest, 0x21, 0x00);

    /// ASC/ASCQ 0x27/0x00 — WRITE PROTECTED.
    pub const WRITE_PROTECTED: Self = Self::new(SenseKey::DataProtect, 0x27, 0x00);

    /// ASC/ASCQ 0x0C/0x00 — WRITE ERROR.
    pub const WRITE_ERROR: Self = Self::new(SenseKey::MediumError, 0x0C, 0x00);

    /// ASC/ASCQ 0x11/0x00 — UNRECOVERED READ ERROR.
    pub const READ_ERROR: Self = Self::new(SenseKey::MediumError, 0x11, 0x00);

    /// ASC/ASCQ 0x26/0x00 — INVALID FIELD IN PARAMETER LIST.
    pub const INVALID_FIELD_IN_PARAMETER_LIST: Self =
        Self::new(SenseKey::IllegalRequest, 0x26, 0x00);

    /// ASC/ASCQ 0x2A/0x09 — CAPACITY DATA HAS CHANGED. UA-class.
    pub const CAPACITY_DATA_HAS_CHANGED: Self = Self::new(SenseKey::UnitAttention, 0x2A, 0x09);

    /// ASC/ASCQ 0x1D/0x00 — MISCOMPARE DURING VERIFY OPERATION.
    pub const MISCOMPARE: Self = Self::new(SenseKey::Miscompare, 0x1D, 0x00);

    /// ASC/ASCQ 0x39/0x00 — SAVING PARAMETERS NOT SUPPORTED.
    pub const SAVING_PARAMETERS_NOT_SUPPORTED: Self =
        Self::new(SenseKey::IllegalRequest, 0x39, 0x00);
}

/// Fluent fixed-format builder — kept as a separate entry point
/// because tape call sites all build sense via this chain
/// (`SenseDataBuilder::new(key, asc).with_eom().build()`). Wraps
/// [`SenseData`]; `.build()` returns ready-to-ship wire bytes.
pub struct SenseDataBuilder(SenseData);

impl SenseDataBuilder {
    /// Create a new fixed-format builder seeded with the given
    /// sense key + ASC/ASCQ pair.
    pub fn new(sense_key: SenseKey, asc_code: AdditionalSenseCode) -> Self {
        Self(SenseData::from_asc(sense_key, asc_code))
    }

    pub fn with_information(mut self, info: u32) -> Self {
        self.0 = self.0.with_information(info);
        self
    }

    pub fn with_command_info(mut self, info: u32) -> Self {
        self.0 = self.0.with_command_info(info);
        self
    }

    pub fn with_ili(mut self) -> Self {
        self.0 = self.0.with_ili();
        self
    }

    pub fn with_eom(mut self) -> Self {
        self.0 = self.0.with_eom();
        self
    }

    pub fn with_filemark(mut self) -> Self {
        self.0 = self.0.with_filemark();
        self
    }

    /// Encode and return the 18-byte fixed-format sense block.
    pub fn build(self) -> Vec<u8> {
        info!(
            "Built sense data: key={:?}, ASC/ASCQ={:02x}/{:02x}, ILI={}, EOM={}, FM={}",
            self.0.key, self.0.asc, self.0.ascq, self.0.ili, self.0.eom, self.0.filemark
        );
        self.0.fixed_bytes_vec()
    }

    /// Return the structured value without encoding. Useful when a
    /// caller wants to feed the value into [`SenseData::to_bytes`]
    /// downstream.
    pub fn into_sense(self) -> SenseData {
        self.0
    }
}

fn decode_sense_key(value: u8) -> Option<SenseKey> {
    Some(match value {
        0x00 => SenseKey::NoSense,
        0x01 => SenseKey::RecoveredError,
        0x02 => SenseKey::NotReady,
        0x03 => SenseKey::MediumError,
        0x04 => SenseKey::HardwareError,
        0x05 => SenseKey::IllegalRequest,
        0x06 => SenseKey::UnitAttention,
        0x07 => SenseKey::DataProtect,
        0x08 => SenseKey::BlankCheck,
        0x09 => SenseKey::VendorSpecific,
        0x0A => SenseKey::CopyAborted,
        0x0B => SenseKey::AbortedCommand,
        0x0C => SenseKey::Equal,
        0x0D => SenseKey::VolumeOverflow,
        0x0E => SenseKey::Miscompare,
        _ => return None,
    })
}

/// Convenience function to build sense data for common errors
pub fn build_sense(sense_key: SenseKey, asc_code: AdditionalSenseCode) -> Vec<u8> {
    SenseDataBuilder::new(sense_key, asc_code).build()
}

/// Build sense data for filemark detection
pub fn build_filemark_sense() -> Vec<u8> {
    SenseDataBuilder::new(SenseKey::NoSense, ASC_FILEMARK_DETECTED)
        .with_filemark()
        .build()
}

/// Build sense data for EOD (End of Data) detection
pub fn build_eod_sense() -> Vec<u8> {
    SenseDataBuilder::new(SenseKey::BlankCheck, ASC_EOD_DETECTED)
        .with_eom()
        .build()
}

/// Build sense data for BOT (Beginning of Tape) detection
pub fn build_bot_sense() -> Vec<u8> {
    SenseDataBuilder::new(SenseKey::NoSense, ASC_BOT_DETECTED).build()
}

/// Build sense data for no medium loaded
pub fn build_no_medium_sense() -> Vec<u8> {
    SenseDataBuilder::new(SenseKey::NotReady, ASC_NOT_READY_NO_MEDIUM).build()
}

/// Build sense data for write protected
pub fn build_write_protected_sense() -> Vec<u8> {
    SenseDataBuilder::new(SenseKey::DataProtect, ASC_WRITE_PROTECTED).build()
}

/// Build sense data for a WORM medium violation (CHECK CONDITION +
/// DATA PROTECT key 0x07 + ASC/ASCQ 0x30/0x0C). Returned by WORM
/// cartridges on WRITE/WRITE FILEMARKS at non-EOD LBAs and on
/// ERASE / FORMAT MEDIUM / ALLOW OVERWRITE.
pub fn build_worm_protected_sense() -> Vec<u8> {
    SenseDataBuilder::new(SenseKey::DataProtect, ASC_WRITE_PROTECTED_WORM).build()
}

/// Build sense data for invalid field in CDB
pub fn build_invalid_field_sense() -> Vec<u8> {
    SenseDataBuilder::new(SenseKey::IllegalRequest, ASC_INVALID_FIELD_IN_CDB).build()
}

/// Build sense data for invalid command
pub fn build_invalid_command_sense() -> Vec<u8> {
    SenseDataBuilder::new(SenseKey::IllegalRequest, ASC_INVALID_COMMAND).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_basic_sense() {
        let sense = SenseDataBuilder::new(SenseKey::NoSense, ASC_NO_ADDITIONAL_INFO).build();

        assert_eq!(sense.len(), 18);
        assert_eq!(sense[0], 0x70); // Response code
        assert_eq!(sense[2], 0x00); // Sense key = NoSense
        assert_eq!(sense[7], 10); // Additional sense length
        assert_eq!(sense[12], 0x00); // ASC
        assert_eq!(sense[13], 0x00); // ASCQ
    }

    #[test]
    fn test_build_filemark_sense() {
        let sense = SenseDataBuilder::new(SenseKey::NoSense, ASC_FILEMARK_DETECTED)
            .with_filemark()
            .build();

        assert_eq!(sense.len(), 18);
        assert_eq!(sense[2] & 0x80, 0x80); // Filemark bit set
        assert_eq!(sense[12], 0x00); // ASC
        assert_eq!(sense[13], 0x01); // ASCQ = filemark
    }

    #[test]
    fn test_build_eom_sense() {
        let sense = SenseDataBuilder::new(SenseKey::BlankCheck, ASC_EOD_DETECTED)
            .with_eom()
            .build();

        assert_eq!(sense[2] & 0x0F, 0x08); // Sense key = BlankCheck
        assert_eq!(sense[2] & 0x40, 0x40); // EOM bit set
        assert_eq!(sense[12], 0x00); // ASC
        assert_eq!(sense[13], 0x05); // ASCQ = EOD
    }

    #[test]
    fn test_build_with_information() {
        let sense = SenseDataBuilder::new(SenseKey::MediumError, ASC_UNRECOVERED_READ_ERROR)
            .with_information(0x12345678)
            .build();

        assert_eq!(sense[0], 0xF0); // Valid bit set
        assert_eq!(sense[3], 0x12);
        assert_eq!(sense[4], 0x34);
        assert_eq!(sense[5], 0x56);
        assert_eq!(sense[6], 0x78);
    }

    #[test]
    fn test_convenience_functions() {
        let sense = build_filemark_sense();
        assert_eq!(sense.len(), 18);
        assert_eq!(sense[2] & 0x80, 0x80);

        let sense = build_no_medium_sense();
        assert_eq!(sense[2] & 0x0F, 0x02); // NotReady
        assert_eq!(sense[12], 0x3A); // ASC = no medium
    }

    #[test]
    fn descriptor_encoding_layout() {
        let s = SenseData::new(SenseKey::IllegalRequest, 0x20, 0x00);
        let b = s.to_descriptor_bytes();
        assert_eq!(b[0], 0x72);
        assert_eq!(b[1], 0x05);
        assert_eq!(b[2], 0x20);
        assert_eq!(b[3], 0x00);
        assert_eq!(b[7], 0x00);
    }

    #[test]
    fn to_bytes_honors_format() {
        let descriptor = SenseData::new(SenseKey::DataProtect, 0x27, 0x00);
        assert_eq!(descriptor.to_bytes().len(), 8);

        let fixed = SenseData::fixed(SenseKey::DataProtect, 0x27, 0x00);
        assert_eq!(fixed.to_bytes().len(), 18);
    }

    #[test]
    fn from_asc_defaults_to_fixed() {
        let s = SenseData::from_asc(SenseKey::NotReady, ASC_NOT_READY_NO_MEDIUM);
        assert_eq!(s.format, SenseFormat::Fixed);
        assert_eq!(s.to_bytes().len(), 18);
    }

    #[test]
    fn builder_into_sense_round_trip() {
        let s = SenseDataBuilder::new(SenseKey::IllegalRequest, ASC_INVALID_FIELD_IN_CDB)
            .with_information(0x42)
            .into_sense();
        assert_eq!(s.format, SenseFormat::Fixed);
        assert_eq!(s.information, Some(0x42));
    }

    #[test]
    fn fixed_bytes_round_trip_through_from_wire_bytes() {
        let raw = SenseDataBuilder::new(SenseKey::MediumError, ASC_UNRECOVERED_READ_ERROR)
            .with_information(0x12345678)
            .with_ili()
            .build();
        let decoded = SenseData::from_wire_bytes(&raw).expect("decode succeeds");
        assert_eq!(decoded.format, SenseFormat::Fixed);
        assert_eq!(decoded.key, SenseKey::MediumError);
        assert_eq!(decoded.asc, 0x11);
        assert_eq!(decoded.ascq, 0x00);
        assert_eq!(decoded.information, Some(0x12345678));
        assert!(decoded.ili);
    }

    #[test]
    fn descriptor_bytes_round_trip_through_from_wire_bytes() {
        let s = SenseData::new(SenseKey::DataProtect, 0x27, 0x00);
        let raw = s.to_descriptor_bytes();
        let decoded = SenseData::from_wire_bytes(&raw).expect("decode succeeds");
        assert_eq!(decoded.format, SenseFormat::Descriptor);
        assert_eq!(decoded.key, SenseKey::DataProtect);
        assert_eq!(decoded.asc, 0x27);
        assert_eq!(decoded.ascq, 0x00);
    }

    #[test]
    fn from_wire_bytes_returns_none_for_short_or_unknown() {
        assert!(SenseData::from_wire_bytes(&[]).is_none());
        assert!(SenseData::from_wire_bytes(&[0xFF, 0, 0, 0]).is_none());
        // Fixed format header but truncated payload.
        assert!(SenseData::from_wire_bytes(&[0x70, 0, 0, 0]).is_none());
    }

    #[test]
    fn fixed_filemark_eom_round_trip() {
        let raw = SenseDataBuilder::new(SenseKey::NoSense, ASC_FILEMARK_DETECTED)
            .with_filemark()
            .with_eom()
            .build();
        let decoded = SenseData::from_wire_bytes(&raw).expect("decode succeeds");
        assert!(decoded.filemark);
        assert!(decoded.eom);
    }
}
