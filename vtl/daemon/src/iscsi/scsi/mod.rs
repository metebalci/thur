// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! SCSI command implementations for thurvtl iSCSI target.
//!
//! Cross-product helpers (sense, log pages) live in the `scsi-ssc`
//! crate and are re-exported here so existing `crate::iscsi::scsi::sense::*`
//! callers resolve unchanged. The `mode_pages` helpers used to be
//! re-exported too — drive-LUN MODE SENSE / MODE SELECT lifted to
//! scsi-ssc in 5.B.6 follow-up step 7, so the wrapper no longer
//! reaches into them. Medium-changer (SMC-3) command builders stay
//! library-local in [`changer`] — they reach into [`core_mediachanger::Library`]
//! directly.

pub use scsi_ssc::scsi::{log_pages, sense};
