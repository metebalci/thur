// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVMe-oF Discovery controller handler.
//!
//! A *direct discovery controller* (NVMe-oF §1.5.7): it answers for
//! the well-known NQN [`nvme_base::identify::DISCOVERY_NQN`] and exposes
//! exactly one log page — the Discovery Log Page (LID 0x70) — listing
//! the I/O subsystems a host can reach. `nvme discover` /
//! `nvme connect-all` read it, then connect to the referenced
//! subsystem NQN directly.
//!
//! This is a separate [`NvmeCommandHandler`] from
//! [`crate::NvmeNvmDispatcher`]: the daemon binds it on its own
//! listener (default port 8009) so it reuses the entire `nvme-tcp`
//! state machine (ICReq → Connect → Property Get/Set → command loop)
//! unchanged. The Connect handler admits the host because the
//! handler's [`subnqn`](NvmeCommandHandler::subnqn) returns the
//! discovery NQN, which the host sends as the Connect SUBNQN.
//!
//! Scope: it has no namespaces and no I/O queues. It answers Identify
//! Controller (CNTLTYPE = Discovery), Get Log Page 0x70, and Keep
//! Alive; Get/Set Features are accepted permissively (a discovery
//! controller has no real feature state to corrupt) so host bring-up
//! doesn't abort; everything else is Invalid Field / Invalid Opcode.
//!
//! The Discovery listener is intentionally cleartext + unauthenticated
//! (the spec/industry default and the analog of our unauthenticated
//! iSCSI SendTargets). The log record advertises the *referenced*
//! subsystem's security requirement via TREQ + TSAS.SECTYPE so the
//! host uses TLS for the real Connect. No volume names leak here —
//! those live behind the I/O-subsystem Connect, whose
//! Active-Namespace-List is admission-fenced.

use std::net::IpAddr;

use async_trait::async_trait;

use nvme_base::identify::{CNS, DISCOVERY_NQN};
use nvme_base::log_page::{self, DiscoveryLogEntry, disc_adrfam, disc_subtype};
use nvme_base::{AdminOpcode, Cqe, IdentifyController, StatusField};

use crate::handler::{AdminCommand, IoCommand, NvmeCommandHandler, NvmeResponse};

/// Generation Counter for the Discovery Log Page. `nvme discover`
/// reads LID 0x70 twice — a header read to learn NUMREC, then the
/// full page — and retries if GENCTR changes between them. The
/// content is fixed at boot, so a constant is correct (and required:
/// a clock-derived value would loop the host forever).
const DISCOVERY_GENCTR: u64 = 0;

/// Minimum admin submission queue size advertised in the log entry's
/// ASQSZ field. 32 is the conventional floor.
const DISCOVERY_ASQSZ: u16 = 32;

/// A Discovery controller that refers hosts to a single NVMe/TCP I/O
/// subsystem. Constructed by the daemon at boot from the resolved
/// NVMe/TCP listener settings.
pub struct DiscoveryHandler {
    /// The referenced I/O subsystem's NQN (what the log entry's SUBNQN
    /// carries, and what the host connects to after discovery).
    io_subnqn: String,
    /// The I/O subsystem's TCP port (log entry TRSVCID).
    io_port: u16,
    /// The I/O subsystem's transport address. `Some` when the operator
    /// bound the I/O listener to a concrete IP; `None` for a wildcard
    /// bind, in which case the log entry reflects the address the
    /// discovery connection actually landed on (`AdminCommand::local_addr`).
    io_traddr: Option<IpAddr>,
    /// TSAS security type the log entry advertises (`disc_sectype::*`).
    sectype: u8,
    /// Transport requirements the log entry advertises (`disc_treq::*`).
    treq: u8,
    /// Identify Controller SN / MN / FR for this discovery controller.
    sn: String,
    mn: String,
    fr: String,
}

impl DiscoveryHandler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        io_subnqn: String,
        io_port: u16,
        io_traddr: Option<IpAddr>,
        sectype: u8,
        treq: u8,
        sn: String,
        mn: String,
        fr: String,
    ) -> Self {
        Self {
            io_subnqn,
            io_port,
            io_traddr,
            sectype,
            treq,
            sn,
            mn,
            fr,
        }
    }

    /// Resolve the transport address to advertise: the configured
    /// concrete I/O IP if set, else the address the discovery
    /// connection arrived on. Falls back to the unspecified IPv4
    /// address only outside a real connection (tests) — a host can't
    /// connect to it, but no real connection lacks a local address.
    fn resolve_traddr(&self, local_addr: Option<std::net::SocketAddr>) -> IpAddr {
        self.io_traddr
            .or_else(|| local_addr.map(|s| s.ip()))
            .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
    }

    /// Build the single-entry Discovery Log Page for this connection.
    fn build_log_page(&self, local_addr: Option<std::net::SocketAddr>) -> Vec<u8> {
        let traddr = self.resolve_traddr(local_addr);
        let adrfam = match traddr {
            IpAddr::V4(_) => disc_adrfam::IPV4,
            IpAddr::V6(_) => disc_adrfam::IPV6,
        };
        let entry = DiscoveryLogEntry {
            adrfam,
            subtype: disc_subtype::NVME,
            treq: self.treq,
            port_id: 1,
            // Dynamic controller — the host requests "any" at Connect.
            cntlid: nvme_base::fabrics::CNTLID_ANY,
            asqsz: DISCOVERY_ASQSZ,
            trsvcid: self.io_port.to_string(),
            subnqn: self.io_subnqn.clone(),
            traddr: traddr.to_string(),
            sectype: self.sectype,
        };
        log_page::discovery_log_page(DISCOVERY_GENCTR, &[entry])
    }

    fn cmd_identify(&self, cmd: &AdminCommand<'_>) -> NvmeResponse {
        let cid = cmd.sqe.cid;
        let raw_cns = (cmd.sqe.cdw10 & 0xFF) as u8;
        match CNS::from_u8(raw_cns) {
            Some(CNS::Controller) => {
                let ic = IdentifyController::discovery(
                    self.sn.clone(),
                    self.mn.clone(),
                    self.fr.clone(),
                    cmd.cntlid.unwrap_or(1),
                );
                NvmeResponse::with_data(Cqe::success(cid, 0, 0, 0), ic.to_bytes().to_vec())
            }
            // A Discovery controller has no namespaces, so Namespace /
            // Active-NS-List / descriptor CNS values are meaningless here.
            _ => NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field())),
        }
    }

    fn cmd_get_log_page(&self, cmd: &AdminCommand<'_>) -> NvmeResponse {
        let sqe = &cmd.sqe;
        let cid = sqe.cid;
        let lid = (sqe.cdw10 & 0xFF) as u8;
        if lid != log_page::lid::DISCOVERY {
            return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_field()));
        }
        // Requested length: CDW10[31:16] NUMDL | CDW11[15:0] NUMDU,
        // zero-based dwords. Offset: CDW12 LPOL | CDW13 LPOU, bytes.
        let numdl = (sqe.cdw10 >> 16) & 0xFFFF;
        let numdu = sqe.cdw11 & 0xFFFF;
        let total_dwords = numdl | (numdu << 16);
        let total_bytes = total_dwords.saturating_add(1).saturating_mul(4) as usize;
        let lpo = (sqe.cdw12 as u64) | ((sqe.cdw13 as u64) << 32);

        let page = self.build_log_page(cmd.local_addr);
        // Honor the Log Page Offset so a host that chunks the read
        // (libnvme splits large logs) still stitches it correctly.
        let start = (lpo as usize).min(page.len());
        let end = start.saturating_add(total_bytes).min(page.len());
        NvmeResponse::with_data(Cqe::success(cid, 0, 0, 0), page[start..end].to_vec())
    }
}

#[async_trait]
impl NvmeCommandHandler for DiscoveryHandler {
    fn subnqn(&self) -> &str {
        DISCOVERY_NQN
    }

    async fn handle_admin(&self, cmd: AdminCommand<'_>) -> NvmeResponse {
        let cid = cmd.sqe.cid;
        let Some(opcode) = AdminOpcode::from_u8(cmd.sqe.opcode) else {
            return NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_opcode()));
        };
        match opcode {
            AdminOpcode::Identify => self.cmd_identify(&cmd),
            AdminOpcode::GetLogPage => self.cmd_get_log_page(&cmd),
            AdminOpcode::KeepAlive => NvmeResponse::just(Cqe::success(cid, 0, 0, 0)),
            // A Discovery controller has no real feature state. Accept
            // Get/Set Features permissively so host bring-up (e.g. an
            // Async Event Configuration write) doesn't abort the
            // session; there is nothing to corrupt.
            AdminOpcode::GetFeatures => NvmeResponse::just(Cqe::success(cid, 0, 0, 0)),
            AdminOpcode::SetFeatures => NvmeResponse::just(Cqe::success(cid, 0, 0, cmd.sqe.cdw11)),
            AdminOpcode::Abort => {
                // DW0 bit 0 = 1: "command was not aborted" (we queue
                // nothing at this layer).
                NvmeResponse::just(Cqe::success(cid, 0, 0, 1))
            }
            // AsyncEventRequest is intercepted by the transport before
            // dispatch; Fabrics is handled by the transport's
            // Property/Connect/Disconnect path. Anything else is not
            // valid against a Discovery controller.
            _ => NvmeResponse::just(Cqe::failure(cid, 0, 0, StatusField::invalid_opcode())),
        }
    }

    async fn handle_io(&self, cmd: IoCommand<'_>) -> NvmeResponse {
        // A Discovery controller has no I/O queues; the host never
        // creates one. Refuse defensively.
        NvmeResponse::just(Cqe::failure(
            cmd.sqe.cid,
            0,
            0,
            StatusField::invalid_opcode(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nvme_base::log_page::{disc_sectype, disc_treq};

    fn sqe(opcode: u8, cdw10: u32) -> nvme_base::Sqe {
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = opcode;
        b[2] = 0x07; // CID
        b[40..44].copy_from_slice(&cdw10.to_le_bytes());
        nvme_base::Sqe::parse(&b).unwrap()
    }

    fn handler(io_traddr: Option<IpAddr>) -> DiscoveryHandler {
        DiscoveryHandler::new(
            "nqn.2025-10.com.metebalci:thurvsa".into(),
            4420,
            io_traddr,
            disc_sectype::NONE,
            disc_treq::NOT_REQUIRED,
            "TESTSN".into(),
            "ThurVSA Discovery".into(),
            "0.1.0".into(),
        )
    }

    fn admin<'a>(s: nvme_base::Sqe, local: Option<std::net::SocketAddr>) -> AdminCommand<'a> {
        AdminCommand {
            sqe: s,
            data_out: None,
            data_in_max: u32::MAX,
            session_volumes: None,
            cntlid: Some(1),
            local_addr: local,
        }
    }

    #[test]
    fn subnqn_is_discovery_nqn() {
        assert_eq!(handler(None).subnqn(), DISCOVERY_NQN);
    }

    #[tokio::test]
    async fn identify_controller_reports_discovery_type() {
        let h = handler(None);
        // CNS = 0x01 (Controller).
        let resp = h
            .handle_admin(admin(sqe(AdminOpcode::Identify as u8, 0x01), None))
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        assert_eq!(resp.data_in.len(), 4096);
        // CNTLTYPE at byte 111 = 2 (Discovery).
        assert_eq!(resp.data_in[111], 2);
    }

    #[tokio::test]
    async fn identify_namespace_cns_is_invalid() {
        let h = handler(None);
        let resp = h
            .handle_admin(admin(sqe(AdminOpcode::Identify as u8, 0x00), None))
            .await;
        assert_eq!(resp.cqe.status, StatusField::invalid_field());
    }

    #[tokio::test]
    async fn discovery_log_uses_configured_concrete_traddr() {
        let ip: IpAddr = "10.0.0.5".parse().unwrap();
        let h = handler(Some(ip));
        // Full page request: header(1024)+entry(1024)=2048 bytes →
        // NUMD zero-based = 511 dwords.
        let resp = h
            .handle_admin(admin(
                sqe(
                    AdminOpcode::GetLogPage as u8,
                    u32::from(log_page::lid::DISCOVERY) | (511u32 << 16),
                ),
                Some("127.0.0.1:8009".parse().unwrap()),
            ))
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        assert_eq!(resp.data_in.len(), 2048);
        // NUMREC @8 = 1.
        assert_eq!(
            u64::from_le_bytes(resp.data_in[8..16].try_into().unwrap()),
            1
        );
        // Entry TRADDR @512..768 reflects the CONFIGURED IP, not the
        // connection's local address.
        assert_eq!(&resp.data_in[1024 + 512..1024 + 512 + 8], b"10.0.0.5");
        // TRSVCID @32..64 = the I/O port.
        assert_eq!(&resp.data_in[1024 + 32..1024 + 36], b"4420");
        // SUBNQN @256 = the I/O subsystem NQN.
        assert_eq!(
            &resp.data_in[1024 + 256..1024 + 256 + 33],
            b"nqn.2025-10.com.metebalci:thurvsa"
        );
    }

    #[tokio::test]
    async fn discovery_log_reflects_local_addr_on_wildcard_bind() {
        let h = handler(None); // wildcard I/O bind
        let resp = h
            .handle_admin(admin(
                sqe(
                    AdminOpcode::GetLogPage as u8,
                    u32::from(log_page::lid::DISCOVERY) | (511u32 << 16),
                ),
                Some("127.0.0.1:8009".parse().unwrap()),
            ))
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
        // TRADDR reflects the address the discovery connection landed on.
        assert_eq!(&resp.data_in[1024 + 512..1024 + 512 + 9], b"127.0.0.1");
    }

    #[tokio::test]
    async fn discovery_log_header_only_read() {
        let h = handler(None);
        // Header-only read: 1024 bytes → NUMD zero-based = 255 dwords.
        let resp = h
            .handle_admin(admin(
                sqe(
                    AdminOpcode::GetLogPage as u8,
                    u32::from(log_page::lid::DISCOVERY) | (255u32 << 16),
                ),
                Some("127.0.0.1:8009".parse().unwrap()),
            ))
            .await;
        assert_eq!(resp.data_in.len(), 1024);
        assert_eq!(
            u64::from_le_bytes(resp.data_in[8..16].try_into().unwrap()),
            1
        );
    }

    #[tokio::test]
    async fn unknown_log_page_is_invalid_field() {
        let h = handler(None);
        let resp = h
            .handle_admin(admin(sqe(AdminOpcode::GetLogPage as u8, 0x02), None))
            .await;
        assert_eq!(resp.cqe.status, StatusField::invalid_field());
    }

    #[tokio::test]
    async fn keep_alive_succeeds() {
        let h = handler(None);
        let resp = h
            .handle_admin(admin(sqe(AdminOpcode::KeepAlive as u8, 0), None))
            .await;
        assert_eq!(resp.cqe.status, StatusField::SUCCESS);
    }

    #[tokio::test]
    async fn io_is_refused() {
        let h = handler(None);
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = 0x02; // NVM Read
        let resp = h
            .handle_io(IoCommand {
                sqe: nvme_base::Sqe::parse(&b).unwrap(),
                data_out: None,
                data_in_max: 4096,
                session_volumes: None,
                host_id: None,
            })
            .await;
        assert_eq!(resp.cqe.status, StatusField::invalid_opcode());
    }
}
