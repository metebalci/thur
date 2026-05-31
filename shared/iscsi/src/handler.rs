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

    /// Whether the transport should collapse the iSCSI ISID to a fixed
    /// constant before it reaches the SCSI layer
    /// ([`ScsiRequest::initiator_isid`]), so persistent reservations key
    /// by initiator IQN alone (issue #57 — see
    /// [`crate::transport::PrInitiatorPort`]). Default `false` (keep the
    /// full IQN + ISID initiator port); each product overrides it from
    /// its `iscsi.reservations.initiator_port` conffile key.
    fn pr_collapse_isid(&self) -> bool {
        false
    }

    /// Run one SCSI command end-to-end. Implementations may pre-/
    /// post-process around the actual dispatch (thurvtl does cloud
    /// chunk prefetch on READ, MOVE MEDIUM legal-hold sentinel
    /// readback, async SEND DIAGNOSTIC self-test, etc. — those
    /// behaviors are thurvtl-internal and don't leak into the
    /// transport).
    async fn dispatch(&self, req: ScsiRequest<'_>) -> ScsiResponse;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU16, Ordering};

    /// Minimal handler exercising the trait surface: the IQN getter,
    /// the default `on_session_close` no-op (overridden here so we can
    /// observe it), and a trivial `dispatch`.
    struct StubHandler {
        iqn: String,
        last_closed_tsih: AtomicU16,
    }

    #[async_trait]
    impl ScsiHandler for StubHandler {
        fn target_iqn(&self) -> &str {
            &self.iqn
        }

        fn on_session_close(&self, tsih: u16, _cid: u16) {
            self.last_closed_tsih.store(tsih, Ordering::SeqCst);
        }

        async fn dispatch(&self, req: ScsiRequest<'_>) -> ScsiResponse {
            // Echo the LUN back through the status so the test can
            // confirm the request threaded through unchanged.
            let mut resp = ScsiResponse::good(Vec::new());
            resp.data_in = vec![req.lun as u8];
            resp
        }
    }

    /// A handler that does not override `on_session_close` — exercises
    /// the trait's default no-op body.
    struct DefaultHookHandler;

    #[async_trait]
    impl ScsiHandler for DefaultHookHandler {
        fn target_iqn(&self) -> &str {
            "iqn.2025-10.com.metebalci:thurvsa"
        }
        async fn dispatch(&self, _req: ScsiRequest<'_>) -> ScsiResponse {
            ScsiResponse::good(Vec::new())
        }
    }

    #[test]
    fn target_iqn_is_returned_verbatim() {
        let h = StubHandler {
            iqn: "iqn.2025-10.com.metebalci:thurvtl".into(),
            last_closed_tsih: AtomicU16::new(0),
        };
        assert_eq!(h.target_iqn(), "iqn.2025-10.com.metebalci:thurvtl");
    }

    #[test]
    fn on_session_close_override_observes_tsih() {
        let h = StubHandler {
            iqn: "x".into(),
            last_closed_tsih: AtomicU16::new(0),
        };
        h.on_session_close(99, 1);
        assert_eq!(h.last_closed_tsih.load(Ordering::SeqCst), 99);
    }

    #[test]
    fn default_on_session_close_is_a_noop() {
        // The default trait body must not panic and must not require
        // an override.
        DefaultHookHandler.on_session_close(7, 0);
    }

    #[tokio::test]
    async fn dispatch_threads_the_request_through() {
        let h = StubHandler {
            iqn: "x".into(),
            last_closed_tsih: AtomicU16::new(0),
        };
        let cdb = [0u8; 16];
        let req = ScsiRequest {
            tsih: 1,
            cid: 0,
            lun: 3,
            cdb: &cdb,
            data_out: &[],
            data_in_max: 0,
            initiator_iqn: None,
            initiator_isid: [0u8; 6],
            peer: "127.0.0.1:1",
            session_partition: None,
            session_volumes: None,
        };
        let resp = h.dispatch(req).await;
        assert_eq!(resp.data_in, vec![3u8]);
    }

    #[tokio::test]
    async fn handler_is_dyn_compatible() {
        // The transport holds an Arc<dyn ScsiHandler>; confirm the
        // trait object resolves and dispatches.
        let h: std::sync::Arc<dyn ScsiHandler> = std::sync::Arc::new(DefaultHookHandler);
        assert_eq!(h.target_iqn(), "iqn.2025-10.com.metebalci:thurvsa");
        let cdb = [0u8; 16];
        let req = ScsiRequest {
            tsih: 0,
            cid: 0,
            lun: 0,
            cdb: &cdb,
            data_out: &[],
            data_in_max: 0,
            initiator_iqn: None,
            initiator_isid: [0u8; 6],
            peer: "p",
            session_partition: None,
            session_volumes: None,
        };
        let resp = h.dispatch(req).await;
        assert!(matches!(resp.status, ScsiStatus::Good));
    }
}
