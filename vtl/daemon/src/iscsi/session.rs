// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Session manager (TSIH / CmdSN / StatSN bookkeeping). Implementation
//! lives in `shared-iscsi`; this module re-exports the surface so
//! existing call sites (`crate::iscsi::session::SessionManager`,
//! `CmdSnVerdict`, `CMDSN_WINDOW`, `SessionInfo`) keep working
//! unchanged.

// `CmdSnVerdict` is consumed by the shared-iscsi transport FFP
// loop after Step 3c phase 2 — no thurvtl-side caller. Only
// `SessionManager` (constructed at boot, queried by the HTTP
// `/sessions` endpoint and the iSCSI server) stays re-exported.
pub use shared_iscsi::session::SessionManager;
