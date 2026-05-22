// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Product-agnostic SCSI handler trait. The shared-iscsi transport
//! ([`crate::transport`]) frames PDUs and runs the connection
//! lifecycle (login, R2T, Data-Out collection, response writeback);
//! everything below the opcode dispatcher lives behind this trait so
//! both products plug in their own SCSI surface:
//!
//! - **thurvtld** — SSC-4 (sequential-access tape) + SMC-3
//!   (medium changer) handlers, a tape-specific SPC-4 surface, and
//!   the existing per-LUN drive / library state.
//! - **thurvsad** — SBC-3 (block / direct-access) handlers
//!   against `VolumeWriter` page I/O.
//!
//! The request / response / status types ([`ScsiRequest`],
//! [`ScsiResponse`], [`ScsiStatus`]) live in `scsi-spc` since
//! Step 5 Milestone 5.A.2; this module re-exports them so the
//! historical `shared_iscsi::ScsiResponse` import path keeps
//! resolving.

use async_trait::async_trait;

pub use scsi_spc::scsi::{ScsiRequest, ScsiResponse, ScsiStatus};

/// Product-agnostic SCSI command handler. Both `thurvtld` (tape /
/// changer) and `thurvsad` (block) implement this trait; the
/// shared-iscsi transport calls [`Self::dispatch`] once per SCSI
/// Command PDU and wraps the result back into the wire protocol.
///
/// The trait is dyn-compatible (boxed `async fn` via `async_trait`)
/// so transports can hold an `Arc<dyn ScsiHandler>` without knowing
/// the concrete product.
#[async_trait]
pub trait ScsiHandler: Send + Sync + 'static {
    /// IQN the transport announces in `TargetName` during login and
    /// SendTargets discovery. thurvtl uses
    /// `iqn.2025-10.com.metebalci:thurvtl`; thurvsa uses
    /// `iqn.2025-10.com.metebalci:thurvsa`.
    fn target_iqn(&self) -> &str;

    /// Hook invoked when an iSCSI session ends (TCP drop, Logout, or
    /// CmdSN-window violation tear-down). Default no-op; the tape
    /// handler overrides to release per-session drive locks and
    /// PREVENT/ALLOW state. thurvsa has no per-session resources to
    /// release today.
    fn on_session_close(&self, _tsih: u16, _cid: u16) {}

    /// Run one SCSI command end-to-end. Implementations may pre-/
    /// post-process around the actual dispatch (thurvtl does cloud
    /// chunk prefetch on READ, MOVE MEDIUM legal-hold sentinel
    /// readback, async SEND DIAGNOSTIC self-test, etc. — those
    /// behaviors are thurvtl-internal and don't leak into the
    /// transport).
    async fn dispatch(&self, req: ScsiRequest<'_>) -> ScsiResponse;
}
