// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Tape-specific SmcError → SCSI sense data mapper.
//!
//! The generic SCSI sense surface (sense keys, ASC/ASCQ table, fixed-
//! format builder, common convenience functions) lives in
//! `shared-iscsi`; this module re-exports those names so existing call
//! sites `crate::iscsi::scsi::sense::*` keep working unchanged. The
//! `error_to_sense` walker stays here because it pattern-matches on
//! `SmcError` (tape-specific variants like `EndOfMedium`,
//! `WormViolation`, `EarlyWarning`, …) and SBC-3 will grow its own
//! when thurvsa starts emitting sense.

#![allow(dead_code)]

// Re-exports of every constant / builder used inside this file's
// `error_to_sense` walker plus the names referenced from elsewhere
// in thurvtld (`AdditionalSenseCode`, `ASC_INVALID_FIELD_IN_CDB`,
// `ASC_LOGICAL_BLOCK_PROTECTION_METHOD_ERROR`, `ASC_MEDIUM_REMOVAL_PREVENTED`,
// `ASC_NO_ADDITIONAL_INFO`, `build_invalid_command_sense`, `build_sense`,
// `build_write_protected_sense`, `SenseDataBuilder`, `SenseKey`).
pub use shared_iscsi::sense::{
    ASC_BOT_DETECTED, ASC_CONDITIONAL_WRITE_PROTECT, ASC_DATA_DECRYPTION_ERROR,
    ASC_ENCRYPTION_KEY_ABSENT, ASC_EOD_DETECTED, ASC_EOM_DETECTED, ASC_FILEMARK_DETECTED,
    ASC_INCOMPATIBLE_MEDIUM_INSTALLED, ASC_INVALID_COMMAND, ASC_INVALID_FIELD_IN_CDB,
    ASC_LBA_OUT_OF_RANGE, ASC_LOGICAL_BLOCK_PROTECTION_METHOD_ERROR, ASC_MEDIUM_REMOVAL_PREVENTED,
    ASC_NO_ADDITIONAL_INFO, ASC_NOT_READY_NO_MEDIUM, ASC_NOT_READY_OPERATION_IN_PROGRESS,
    ASC_UNRECOVERED_READ_ERROR, ASC_WRITE_PROTECTED, ASC_WRITE_PROTECTED_WORM, AdditionalSenseCode,
    SenseDataBuilder, SenseKey, build_sense, build_write_protected_sense,
};

/// Map SmcError to sense data
/// This provides automatic conversion from core error types to SCSI sense data
pub fn error_to_sense(error: &core_mediachanger::errors::SmcError) -> Vec<u8> {
    use core_mediachanger::errors::SmcError;

    match error {
        // Not Ready conditions
        SmcError::NoCartridgeLoaded(_) => {
            SenseDataBuilder::new(SenseKey::NotReady, ASC_NOT_READY_NO_MEDIUM).build()
        }

        // Tape position conditions (with special flags). EOD is *not*
        // physical end-of-medium — SSC-4 §8.3.1 reserves the EOM bit
        // for the VolumeOverflow / EarlyWarning paths below. Handlers
        // that own CDB context (`handle_read_6`) build their own sense
        // with INFORMATION = residual; this fallback is for paths
        // without a transfer length to populate.
        SmcError::EndOfData => {
            SenseDataBuilder::new(SenseKey::BlankCheck, ASC_EOD_DETECTED).build()
        }
        SmcError::BeginningOfTape => {
            SenseDataBuilder::new(SenseKey::NoSense, ASC_BOT_DETECTED).build()
        }
        SmcError::FilemarkDetected => {
            SenseDataBuilder::new(SenseKey::NoSense, ASC_FILEMARK_DETECTED)
                .with_filemark()
                .build()
        }
        SmcError::EndOfMedium => SenseDataBuilder::new(SenseKey::VolumeOverflow, ASC_EOM_DETECTED)
            .with_eom()
            .build(),
        // Early warning: data is on the medium but we just crossed
        // the 95% threshold. SCSI convention: NoSense + EOM=1 +
        // 0x00/0x02 — host treats the response as success-with-warning
        // and typically finishes the current append before unload.
        SmcError::EarlyWarning => SenseDataBuilder::new(SenseKey::NoSense, ASC_EOM_DETECTED)
            .with_eom()
            .build(),

        // Data Protect conditions
        SmcError::WriteProtected => {
            SenseDataBuilder::new(SenseKey::DataProtect, ASC_WRITE_PROTECTED).build()
        }
        SmcError::WormViolation => {
            SenseDataBuilder::new(SenseKey::DataProtect, ASC_WRITE_PROTECTED_WORM).build()
        }
        // Legal hold: plain WRITE PROTECTED 0x27/0x00 — operator-applied
        // preservation, not sticky-at-create WORM. Backup software
        // sees a stable "this medium is read-only" signal.
        SmcError::LegalHoldViolation => {
            SenseDataBuilder::new(SenseKey::DataProtect, ASC_WRITE_PROTECTED).build()
        }
        // Encrypt-Only mode active without an installed encryption
        // key. Use the SPC-4 "ENCRYPTION KEY ABSENT" code so the host
        // can distinguish "no key set" from generic write-protect.
        SmcError::EncryptOnlyKeyAbsent => {
            SenseDataBuilder::new(SenseKey::DataProtect, ASC_ENCRYPTION_KEY_ABSENT).build()
        }
        // Append-Only mode rejected a non-EOD write. CONDITIONAL
        // WRITE PROTECT (0x27/0x06) — distinct from WORM's 0x30/0x0C
        // because it's session-scoped, not sticky-at-create.
        SmcError::AppendOnlyMustExtendEod => {
            SenseDataBuilder::new(SenseKey::DataProtect, ASC_CONDITIONAL_WRITE_PROTECT).build()
        }

        // Illegal Request conditions
        SmcError::InvalidField => {
            SenseDataBuilder::new(SenseKey::IllegalRequest, ASC_INVALID_FIELD_IN_CDB).build()
        }
        // Incompatible medium: cartridge generation > drive generation,
        // or LTO-7 Type M into an LTO-7 drive.
        SmcError::IncompatibleMedium { .. } => {
            SenseDataBuilder::new(SenseKey::IllegalRequest, ASC_INCOMPATIBLE_MEDIUM_INSTALLED)
                .build()
        }
        SmcError::InvalidCommand => {
            SenseDataBuilder::new(SenseKey::IllegalRequest, ASC_INVALID_COMMAND).build()
        }
        SmcError::LbaOutOfRange => {
            SenseDataBuilder::new(SenseKey::IllegalRequest, ASC_LBA_OUT_OF_RANGE).build()
        }

        // Medium Error conditions
        SmcError::VerifyFailed { lba, .. } => {
            SenseDataBuilder::new(SenseKey::MediumError, ASC_UNRECOVERED_READ_ERROR)
                .with_information(*lba as u32)
                .build()
        }
        // Chunk integrity failure on cloud refetch: same UNRECOVERED
        // READ ERROR (0x11/0x00) the LBA-keyed VerifyFailed uses, no
        // information field (the corrupted chunk spans many LBAs).
        // Backup software (Veeam / NetBackup / tar / Bacula) treats
        // this as a per-block read failure and logs + skips, instead
        // of failing the whole tape job.
        SmcError::ContentHashMismatch { .. } => {
            SenseDataBuilder::new(SenseKey::MediumError, ASC_UNRECOVERED_READ_ERROR).build()
        }

        // Session/drive management errors (map to hardware error)
        SmcError::InvalidSession(_) => SenseDataBuilder::new(
            SenseKey::HardwareError,
            AdditionalSenseCode {
                asc: 0x44,
                ascq: 0x00,
            },
        )
        .build(),
        SmcError::DriveReserved(_) => {
            // SCSI BUSY status should be returned by caller, but provide sense data
            SenseDataBuilder::new(
                SenseKey::NotReady,
                AdditionalSenseCode {
                    asc: 0x04,
                    ascq: 0x03, // Not ready - manual intervention required
                },
            )
            .build()
        }
        SmcError::InvalidDrive(_) => {
            SenseDataBuilder::new(SenseKey::IllegalRequest, ASC_INVALID_FIELD_IN_CDB).build()
        }

        // Changer errors
        SmcError::InvalidElementType => {
            SenseDataBuilder::new(SenseKey::IllegalRequest, ASC_INVALID_FIELD_IN_CDB).build()
        }

        // Generic errors
        SmcError::InvalidOp(_) => {
            SenseDataBuilder::new(SenseKey::IllegalRequest, ASC_INVALID_COMMAND).build()
        }

        // Daemon lock error (should never occur in daemon context, but map to hardware error)
        SmcError::DaemonRunning(_) => SenseDataBuilder::new(
            SenseKey::HardwareError,
            AdditionalSenseCode {
                asc: 0x44,
                ascq: 0x00,
            },
        )
        .build(),
        SmcError::Io(_) => SenseDataBuilder::new(
            SenseKey::HardwareError,
            AdditionalSenseCode {
                asc: 0x44,
                ascq: 0x00,
            },
        )
        .build(),
        SmcError::SerdeJson(_) => SenseDataBuilder::new(
            SenseKey::HardwareError,
            AdditionalSenseCode {
                asc: 0x44,
                ascq: 0x00,
            },
        )
        .build(),
        SmcError::ObjectStoreError(_) => SenseDataBuilder::new(
            SenseKey::HardwareError,
            AdditionalSenseCode {
                asc: 0x44,
                ascq: 0x00,
            },
        )
        .build(),
        SmcError::AuthFailed(_) => {
            // Authentication failures shouldn't reach SCSI layer, but handle anyway
            SenseDataBuilder::new(
                SenseKey::IllegalRequest,
                AdditionalSenseCode {
                    asc: 0x44,
                    ascq: 0x00,
                },
            )
            .build()
        }
        SmcError::CompressionError(_) => {
            // Compression errors are internal errors
            SenseDataBuilder::new(
                SenseKey::HardwareError,
                AdditionalSenseCode {
                    asc: 0x44,
                    ascq: 0x00,
                },
            )
            .build()
        }
        SmcError::ConfigError(_) => {
            // Configuration errors are internal errors
            SenseDataBuilder::new(
                SenseKey::HardwareError,
                AdditionalSenseCode {
                    asc: 0x44,
                    ascq: 0x00,
                },
            )
            .build()
        }
        SmcError::LibraryConfig(_) => {
            // Library configuration errors are internal errors
            SenseDataBuilder::new(
                SenseKey::HardwareError,
                AdditionalSenseCode {
                    asc: 0x44,
                    ascq: 0x00,
                },
            )
            .build()
        }
        SmcError::CartridgeNotFound(_) => {
            // Cartridge not found - medium not present
            // ASC 0x3A = Medium not present
            SenseDataBuilder::new(
                SenseKey::NotReady,
                AdditionalSenseCode {
                    asc: 0x3A,
                    ascq: 0x00,
                },
            )
            .build()
        }
        // Drive-level encryption errors (LTO Application-Managed Encryption)
        SmcError::DataDecryptionError(_) => {
            // SSC-4: DATA DECRYPTION ERROR — drive can't decrypt with current
            // key, OR the host tried to read an encrypted block without one.
            SenseDataBuilder::new(SenseKey::DataProtect, ASC_DATA_DECRYPTION_ERROR).build()
        }
        SmcError::EncryptionError(_) => SenseDataBuilder::new(
            SenseKey::HardwareError,
            AdditionalSenseCode {
                asc: 0x44,
                ascq: 0x00,
            },
        )
        .build(),
        SmcError::NotSupported(_) => {
            SenseDataBuilder::new(SenseKey::IllegalRequest, ASC_INVALID_COMMAND).build()
        }
        SmcError::Backpressured(_) => {
            // Transient: pool full, host should retry. Real LTO drives
            // surface this when their internal buffer is overcommitted.
            SenseDataBuilder::new(SenseKey::NotReady, ASC_NOT_READY_OPERATION_IN_PROGRESS).build()
        }
        // `SmcError` is `#[non_exhaustive]`; new variants from a
        // future revision get a generic Internal Target Failure here
        // until they earn a tailored sense mapping.
        _ => SenseDataBuilder::new(
            SenseKey::HardwareError,
            AdditionalSenseCode {
                asc: 0x44,
                ascq: 0x00,
            },
        )
        .build(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_to_sense_not_ready() {
        use core_mediachanger::errors::SmcError;

        let error = SmcError::NoCartridgeLoaded(0);
        let sense = error_to_sense(&error);

        assert_eq!(sense.len(), 18);
        assert_eq!(sense[2] & 0x0F, 0x02); // NotReady
        assert_eq!(sense[12], 0x3A); // ASC = no medium
        assert_eq!(sense[13], 0x00); // ASCQ
    }

    #[test]
    fn test_error_to_sense_filemark() {
        use core_mediachanger::errors::SmcError;

        let error = SmcError::FilemarkDetected;
        let sense = error_to_sense(&error);

        assert_eq!(sense.len(), 18);
        assert_eq!(sense[2] & 0x0F, 0x00); // NoSense
        assert_eq!(sense[2] & 0x80, 0x80); // Filemark bit set
        assert_eq!(sense[12], 0x00); // ASC
        assert_eq!(sense[13], 0x01); // ASCQ = filemark
    }

    #[test]
    fn test_error_to_sense_eod() {
        use core_mediachanger::errors::SmcError;

        let error = SmcError::EndOfData;
        let sense = error_to_sense(&error);

        assert_eq!(sense.len(), 18);
        assert_eq!(sense[2] & 0x0F, 0x08); // BlankCheck
        assert_eq!(sense[2] & 0x40, 0x00); // EOM bit clear (EOD != physical EOM)
        assert_eq!(sense[12], 0x00); // ASC
        assert_eq!(sense[13], 0x05); // ASCQ = EOD
    }

    #[test]
    fn test_error_to_sense_write_protected() {
        use core_mediachanger::errors::SmcError;

        let error = SmcError::WriteProtected;
        let sense = error_to_sense(&error);

        assert_eq!(sense.len(), 18);
        assert_eq!(sense[2] & 0x0F, 0x07); // DataProtect
        assert_eq!(sense[12], 0x27); // ASC = write protected
        assert_eq!(sense[13], 0x00); // ASCQ
    }

    #[test]
    fn test_error_to_sense_invalid_field() {
        use core_mediachanger::errors::SmcError;

        let error = SmcError::InvalidField;
        let sense = error_to_sense(&error);

        assert_eq!(sense.len(), 18);
        assert_eq!(sense[2] & 0x0F, 0x05); // IllegalRequest
        assert_eq!(sense[12], 0x24); // ASC = invalid field
        assert_eq!(sense[13], 0x00); // ASCQ
    }

    #[test]
    fn test_error_to_sense_verify_failed_carries_lba() {
        use core_mediachanger::errors::SmcError;

        let error = SmcError::VerifyFailed {
            lba: 0x12345678,
            expected: "expected".into(),
            actual: "actual".into(),
        };
        let sense = error_to_sense(&error);

        assert_eq!(sense.len(), 18);
        assert_eq!(sense[0], 0xF0); // Valid bit set
        assert_eq!(sense[2] & 0x0F, 0x03); // MediumError
        assert_eq!(sense[12], 0x11); // ASC = unrecovered read error
        assert_eq!(sense[13], 0x00); // ASCQ
        // Information field carries the failing LBA.
        assert_eq!(sense[3], 0x12);
        assert_eq!(sense[4], 0x34);
        assert_eq!(sense[5], 0x56);
        assert_eq!(sense[6], 0x78);
    }

    #[test]
    fn test_error_to_sense_bot() {
        use core_mediachanger::errors::SmcError;

        let error = SmcError::BeginningOfTape;
        let sense = error_to_sense(&error);

        assert_eq!(sense.len(), 18);
        assert_eq!(sense[2] & 0x0F, 0x00); // NoSense
        assert_eq!(sense[12], 0x00); // ASC
        assert_eq!(sense[13], 0x04); // ASCQ = BOT
    }
}
