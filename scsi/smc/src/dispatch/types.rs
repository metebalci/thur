// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-command context for changer-LUN SCSI dispatch.
//!
//! [`SmcScsiCtx`] wraps the shared `ScsiCtx` (from `scsi-ssc`) and
//! adds the SMC-side borrows: the `Library` mutex (slot / drive /
//! mail state) and the [`ElementAddressConfig`](crate::changer::ElementAddressConfig)
//! (element address → slot / drive / mail mapping). Built once per
//! SCSI command in `thurvtld::iscsi::protocol::handle_scsi_command`.
//!
//! Both the lifted changer handlers in [`super::handlers`] and the
//! daemon-local LUN-0 INQUIRY / LOG SENSE / MODE SENSE handlers
//! consume `&mut SmcScsiCtx<'_>` — Deref/DerefMut let either side
//! reach the shared `ScsiCtx` fields without re-naming.

use core_mediachanger::Library;
use scsi_ssc::dispatch::ScsiCtx;
use shared_iscsi::session::SessionManager;
use std::sync::{Arc, Mutex};

use crate::changer::ElementAddressConfig;

pub struct SmcScsiCtx<'a> {
    pub inner: ScsiCtx<'a>,
    pub library: &'a Arc<Mutex<Library>>,
    pub element_config: &'a ElementAddressConfig,
    /// Live iSCSI session registry, used by MOVE / EXCHANGE MEDIUM to
    /// raise MEDIUM MAY HAVE CHANGED on every initiator's drive LUN —
    /// not just the session that issued the changer command (issue #190).
    pub session_manager: &'a Arc<SessionManager>,
}

impl<'a> std::ops::Deref for SmcScsiCtx<'a> {
    type Target = ScsiCtx<'a>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for SmcScsiCtx<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
