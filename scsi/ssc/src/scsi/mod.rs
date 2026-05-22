// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! SCSI command-set helper modules shared by both tape products.
//! Library-only `changer.rs` stays in `thurvtld` —
//! these modules deliberately avoid reaching into `core_mediachanger::Library`.

pub mod attributes;
pub mod encryption_pages;
pub mod log_pages;
pub mod mode_pages;
pub mod sense;
