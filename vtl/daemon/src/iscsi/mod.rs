// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// iSCSI Target Module
//
// This module contains the complete iSCSI target implementation,
// including protocol handling, SCSI command processing, and session management.

pub mod auth;
pub mod config;
pub mod handler;
pub mod protocol;
pub mod scsi;
pub mod server;
pub mod session;
pub mod unit_attention;

// `drive_manager` lifted into `scsi-ssc` (5.B.6 follow-up) — re-export
// the module path so existing `super::drive_manager::*` call sites in
// `protocol.rs` / `handler.rs` resolve unchanged.
pub use scsi_ssc::drive_manager;

// Re-export commonly used types. `IscsiLibraryHandler` stays scoped
// to the `handler` module — only `server::IscsiServer::run`
// constructs it, and external callers use the `IscsiServer` facade.
pub use server::IscsiServer;
