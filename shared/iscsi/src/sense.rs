// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Re-export shell for the shared SPC-4 sense surface.
//!
//! The actual sense types (`SenseKey`, `AdditionalSenseCode`, the
//! ASC/ASCQ table, `SenseData`, `SenseDataBuilder`, the
//! convenience `build_*_sense` helpers) moved to
//! [`scsi_spc::sense`] in Step 5 Milestone 5.A.2. This file
//! keeps the historical `shared_iscsi::sense::*` import path
//! working unchanged — every callable name re-exports verbatim.

pub use scsi_spc::sense::*;
