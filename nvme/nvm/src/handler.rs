// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Transport-facing handler trait.
//!
//! `nvme-tcp`'s server loop parses an incoming CapsuleCmd PDU into
//! an SQE, collects any host-to-controller data (R2T + H2CData
//! flow), and then hands the result to a handler implementing this
//! trait. The trait returns a [`NvmeResponse`]: a CQE plus optional
//! data-in payload the transport wraps into C2HData PDUs.
//!
//! Splitting admin vs I/O at the trait boundary makes future
//! command-set additions (e.g. ZNS, KV) clean: each is its own
//! handler implementing the same trait.

use async_trait::async_trait;

use nvme_base::{Cqe, Sqe, StatusField};

/// Admin command coming in on the admin queue (qid = 0). The
/// transport hands the parsed SQE; this layer dispatches on
/// `sqe.opcode` (interpreted as an `AdminOpcode`).
pub struct AdminCommand<'a> {
    pub sqe: Sqe,
    /// Host-to-controller payload, already collected by the
    /// transport. `None` if the command has no data-out phase.
    pub data_out: Option<&'a [u8]>,
    /// Caller-supplied capacity for the data-in payload. The handler
    /// returns at most this many bytes.
    pub data_in_max: u32,
    /// Per-connection admission set (volume names this hostnqn is
    /// admitted to). `None` = no fence (legacy / no PSK admission
    /// entry). Resolved at Fabrics Connect from
    /// `nvmetcp-psks.json::PskEntry.volumes`.
    pub session_volumes: Option<&'a [String]>,
    /// CNTLID of the controller this admin queue drives, assigned at
    /// Fabrics Connect. Keys the per-controller AER state (Identify
    /// Controller, FID 0x82 mask, LID 0x80 log). `None` only outside a
    /// real connection (tests).
    pub cntlid: Option<u16>,
}

/// I/O command coming in on a non-zero queue (qid > 0). Same shape
/// as [`AdminCommand`]; the type split is purely a routing hint —
/// queues are typed at create time and the transport never crosses
/// them.
pub struct IoCommand<'a> {
    pub sqe: Sqe,
    pub data_out: Option<&'a [u8]>,
    pub data_in_max: u32,
    /// See `AdminCommand::session_volumes`.
    pub session_volumes: Option<&'a [String]>,
    /// 128-bit Host Identifier from the Fabrics Connect data. Names the
    /// reservation registrant (one HOSTID spans the host's many
    /// connections). `None` only outside a real connection (tests).
    pub host_id: Option<[u8; 16]>,
}

/// Handler reply. The CQE is always present; the data-in payload
/// is empty for write-style commands and Identify-less admin paths.
pub struct NvmeResponse {
    pub cqe: Cqe,
    pub data_in: Vec<u8>,
}

impl NvmeResponse {
    pub fn just(cqe: Cqe) -> Self {
        Self {
            cqe,
            data_in: Vec::new(),
        }
    }

    pub fn with_data(cqe: Cqe, data_in: Vec<u8>) -> Self {
        Self { cqe, data_in }
    }
}

/// What the transport calls. Two methods — one per queue type. The
/// trait is dyn-compatible (boxed `async fn` via `async_trait`) so
/// transports can hold `Arc<dyn NvmeCommandHandler>` without knowing
/// the concrete command-set.
#[async_trait]
pub trait NvmeCommandHandler: Send + Sync + 'static {
    /// NVMe Subsystem NQN this handler responds for. Used by the
    /// transport's Connect handler to match against the host's
    /// `subnqn` field.
    fn subnqn(&self) -> &str;

    /// Run one admin command. The transport allocates the CQE
    /// SQHD / SQID / CID values from the handler's reply.
    async fn handle_admin(&self, cmd: AdminCommand<'_>) -> NvmeResponse;

    /// Run one I/O command.
    async fn handle_io(&self, cmd: IoCommand<'_>) -> NvmeResponse;

    /// Handle a fused Compare+Write pair (NVM Command Set §3.2.5).
    /// The transport buffers the first (Compare) SQE + its assembled
    /// data, then invokes this method when the second (Write) SQE
    /// arrives. Per NVMe Base §4.2.6 the controller may return one
    /// CQE per command — we return both so host queue tracking stays
    /// clean.
    ///
    /// Default impl returns `Invalid Opcode` for both halves; concrete
    /// command-set handlers (e.g. [`crate::NvmeNvmDispatcher`])
    /// override to route through `PageCache::compare_and_write_bytes`.
    async fn handle_fused_compare_write(
        &self,
        compare: IoCommand<'_>,
        write: IoCommand<'_>,
    ) -> (Cqe, Cqe) {
        let _ = (compare.data_out, write.data_out);
        (
            Cqe::failure(compare.sqe.cid, 0, 0, StatusField::invalid_opcode()),
            Cqe::failure(write.sqe.cid, 0, 0, StatusField::invalid_opcode()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqe_with_cid(cid: u16) -> Sqe {
        let mut buf = [0u8; nvme_base::SQE_SIZE];
        // CDW0[31:16] carries the Command Identifier — LE bytes 2..4.
        buf[2..4].copy_from_slice(&cid.to_le_bytes());
        Sqe::parse(&buf).expect("64-byte SQE parses")
    }

    fn io_command(cid: u16) -> IoCommand<'static> {
        IoCommand {
            sqe: sqe_with_cid(cid),
            data_out: None,
            data_in_max: 0,
            session_volumes: None,
            host_id: None,
        }
    }

    #[test]
    fn response_just_carries_an_empty_data_in() {
        let resp = NvmeResponse::just(Cqe::success(7, 0, 0, 0));
        assert_eq!(resp.cqe.cid, 7);
        assert!(resp.data_in.is_empty());
    }

    #[test]
    fn response_with_data_keeps_the_payload() {
        let resp = NvmeResponse::with_data(Cqe::success(1, 0, 0, 0), vec![9, 8, 7]);
        assert_eq!(resp.cqe.cid, 1);
        assert_eq!(resp.data_in, vec![9, 8, 7]);
    }

    /// Handler that does not override `handle_fused_compare_write`, so
    /// the trait's default impl is what the test below exercises.
    struct StubHandler;

    #[async_trait]
    impl NvmeCommandHandler for StubHandler {
        fn subnqn(&self) -> &str {
            "nqn.test"
        }
        async fn handle_admin(&self, cmd: AdminCommand<'_>) -> NvmeResponse {
            NvmeResponse::just(Cqe::success(cmd.sqe.cid, 0, 0, 0))
        }
        async fn handle_io(&self, cmd: IoCommand<'_>) -> NvmeResponse {
            NvmeResponse::just(Cqe::success(cmd.sqe.cid, 0, 0, 0))
        }
    }

    #[tokio::test]
    async fn default_fused_compare_write_fails_both_halves() {
        let handler = StubHandler;
        assert_eq!(handler.subnqn(), "nqn.test");

        let (compare_cqe, write_cqe) = handler
            .handle_fused_compare_write(io_command(0x11), io_command(0x22))
            .await;

        // Each half's CQE echoes its own command identifier.
        assert_eq!(compare_cqe.cid, 0x11);
        assert_eq!(write_cqe.cid, 0x22);

        // The default impl rejects the unsupported fused pair.
        let invalid = StatusField::invalid_opcode().to_u16();
        assert_eq!(compare_cqe.status.to_u16(), invalid);
        assert_eq!(write_cqe.status.to_u16(), invalid);
    }
}
