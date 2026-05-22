// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVMe/TCP per-connection state machine.
//!
//! Three phases, in order:
//!
//! 1. **ICReq → ICResp** (NVMe/TCP §3.6.1 / §3.6.2). The very first
//!    PDU on every accepted TCP connection MUST be ICReq. We
//!    negotiate digests off (DGST=0) and an `MAXH2CDATA` large
//!    enough that hosts have no reason to fall back to R2T —
//!    the MVP server only handles in-capsule writes.
//! 2. **Connect** (NVMe-oF §6.3.1). The first CapsuleCmd carries an
//!    Admin Fabrics command with FCTYPE=0x01. We validate the
//!    host's SUBNQN against our subsystem's NQN, assign CNTLID=1,
//!    and capture QID from CDW10[31:16]. QID=0 means this
//!    connection drives the admin queue; QID>0 means an I/O queue.
//! 3. **Command loop**. Each subsequent CapsuleCmd routes through
//!    [`NvmeCommandHandler::handle_admin`] or
//!    [`NvmeCommandHandler::handle_io`] based on the captured QID.
//!    Responses with `data_in.len() > 0` get a preceding C2HData
//!    PDU; every command gets a CapsuleResp.
//!
//! Errors on the wire (unexpected PDU type, malformed Connect Data,
//! SUBNQN mismatch) are reported via C2HTermReq with the appropriate
//! Fatal Error Status (FES) and the connection is dropped. Clean
//! host-driven teardown (H2CTermReq) closes silently.
//!
//! Out of scope this session (per the build-up roadmap in
//! `docs/NVMETCP.md` § Follow-up scope): R2T flow control,
//! header / data digests (CRC32C), TLS-PSK, fused Compare+Write
//! pairing, Property Get / Set, Disconnect, Authentication Send /
//! Receive.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};

use nvme_base::{
    AdminOpcode, ConnectData, ControllerRegs, Cqe, FabricsType, Fuse, Sqe, StatusField,
};
use nvme_nvm::{AdminCommand, IoCommand, NvmeCommandHandler};

use crate::pdu;
use crate::tls::{NvmePskAcceptor, parse_psk_identity};

/// Server boot config.
pub struct ServerConfig {
    pub listen_address: String,
    pub handler: Arc<dyn NvmeCommandHandler>,
    /// Controller register state (CC / CSTS / VS / CAP) shared
    /// across every connection bound to the same controller. The
    /// daemon constructs one via `Arc::new(ControllerRegs::new())`
    /// at boot and hands the same Arc to every transport it spins
    /// up. Tests can inject a fresh one per server.
    pub controller_regs: Arc<ControllerRegs>,
    /// Optional TLS 1.3 PSK acceptor (NVMe-TCP §3.6.1.5). When set,
    /// every accepted TCP connection is wrapped in TLS before the
    /// NVMe ICReq/Connect handshake runs. Built by
    /// [`crate::tls::build_psk_acceptor`] from the daemon-loaded
    /// PSK table.
    pub tls: Option<NvmePskAcceptor>,
}

/// Bind the configured TCP listen address and accept-loop forever.
/// One spawned task per accepted connection.
pub async fn run(config: ServerConfig) -> Result<()> {
    let listener = TcpListener::bind(&config.listen_address).await?;
    tracing::info!(
        listen = %config.listen_address,
        subnqn = %config.handler.subnqn(),
        tls = config.tls.is_some(),
        "nvme-tcp: listener bound",
    );
    accept_loop(
        listener,
        config.handler,
        config.controller_regs,
        config.tls.map(Arc::new),
    )
    .await
}

/// Accept-loop body factored out so tests can supply their own
/// pre-bound listener (e.g. `127.0.0.1:0` to let the kernel pick a
/// free port).
pub async fn accept_loop(
    listener: TcpListener,
    handler: Arc<dyn NvmeCommandHandler>,
    controller_regs: Arc<ControllerRegs>,
    tls: Option<Arc<NvmePskAcceptor>>,
) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let handler = Arc::clone(&handler);
        let regs = Arc::clone(&controller_regs);
        let tls = tls.clone();
        tokio::spawn(async move {
            let result = match tls {
                None => serve_connection(stream, peer, handler, regs, None).await,
                Some(acceptor) => match acceptor.accept(stream).await {
                    Ok(tls_stream) => {
                        // Capture the PSK identity the host used so
                        // serve_connection can cross-check it against
                        // the Connect command's HostNQN.
                        let tls_host_nqn = extract_negotiated_host_nqn(&tls_stream, peer);
                        serve_connection(tls_stream, peer, handler, regs, tls_host_nqn).await
                    }
                    Err(e) => {
                        tracing::warn!(peer = %peer, error = %e, "nvme-tcp: TLS handshake failed");
                        return;
                    }
                },
            };
            if let Err(e) = result {
                tracing::warn!(peer = %peer, error = %e, "nvme-tcp: connection error");
            }
        });
    }
}

/// Pull the negotiated PSK identity off a freshly-accepted
/// `TlsStream`, parse it, and return the host NQN field. Logged on
/// parse failure but never fatal — the Connect-time cross-check
/// will catch a mismatch and a `None` here just means "no
/// cross-check available."
fn extract_negotiated_host_nqn<S>(
    tls_stream: &s2n_tls_tokio::TlsStream<S, s2n_tls::connection::Connection>,
    peer: std::net::SocketAddr,
) -> Option<String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use s2n_tls::connection::Connection;
    let conn: &Connection = tls_stream.as_ref();
    let len = conn.negotiated_psk_identity_length().ok()?;
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    conn.negotiated_psk_identity(&mut buf).ok()?;
    let s = std::str::from_utf8(&buf).ok()?;
    match parse_psk_identity(s) {
        Ok(parsed) => Some(parsed.hostnqn.to_string()),
        Err(e) => {
            tracing::warn!(
                peer = %peer,
                error = %e,
                "nvme-tcp: TLS PSK identity unparseable (cross-check disabled for this connection)",
            );
            None
        }
    }
}

/// MAXH2CDATA advertised in ICResp. Per NVMe/TCP §3.6.2, also the
/// hard cap on individual H2CData PDU payload size on this
/// connection — the host MUST chunk larger transfers across
/// multiple H2CData PDUs. 128 KiB matches typical kernel host
/// defaults; receive cap [`pdu::MAX_PDU_BYTES`] is 256 KiB so we
/// can still accept this in one PDU plus headers.
const ADVERTISED_MAXH2CDATA: u32 = 128 * 1024;

/// Direction of host-visible data transfer on an NVMe command,
/// derived from the low two bits of the opcode (NVMe Base §4.2.1
/// Figure 138). Drives whether the server expects in-capsule data /
/// runs an R2T flow / sends C2HData / does no data movement at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataDirection {
    None,
    HostToController,
    ControllerToHost,
    Bidirectional,
}

fn data_direction(opc: u8) -> DataDirection {
    match opc & 0b11 {
        0b00 => DataDirection::None,
        0b01 => DataDirection::HostToController,
        0b10 => DataDirection::ControllerToHost,
        _ => DataDirection::Bidirectional,
    }
}

/// FES codes (NVMe/TCP §3.6.4) we emit. Only the subset the MVP
/// can hit; full enumeration in the spec.
mod fes {
    /// Invalid PDU Header Field — e.g. PFV != 0 in ICReq.
    pub const INVALID_PDU_HEADER_FIELD: u16 = 0x01;
    /// PDU Sequence Error — wrong PDU type for this phase
    /// (e.g. CapsuleCmd before Connect succeeds).
    pub const PDU_SEQUENCE_ERROR: u16 = 0x02;
    /// Invalid PDU Header Type — opcode byte we don't recognize.
    pub const INVALID_PDU_HEADER_TYPE: u16 = 0x07;
}

/// Max concurrent in-flight commands per connection. Tracked as the
/// size of the per-connection `CommandTable` (R2T-needing writes are
/// the only commands that consume the table). Comfortable headroom
/// under `CAP.MQES=1024`; if a host queues more, the 257th gets a
/// `Namespace Not Ready` CQE instead of a spawned task.
const INFLIGHT_CAP: usize = 256;

/// Per-command H2CData routing channel depth. With
/// `ADVERTISED_MAXH2CDATA=128 KiB` and the host's MAXR2T effectively
/// 1 (we don't multi-R2T), 4 PDUs is plenty of headroom for back-
/// to-back H2CData chunks before the per-command task drains them.
const PER_COMMAND_H2C_CAPACITY: usize = 4;

/// Outbound channel depth. Bounded so a slow writer back-pressures
/// per-command tasks at PDU emission instead of letting unbounded
/// outbound buffering hide the stall.
const OUTBOUND_CAPACITY: usize = 64;

/// Per-command H2CData chunk forwarded from the reader (which parses
/// off the wire) to the matching per-command task (which assembles
/// the full write buffer at the right offsets). Already validated
/// CCCID/TTAG/header by the reader.
#[derive(Debug)]
struct H2CDataChunk {
    ttag: u16,
    datao: u32,
    data: Vec<u8>,
    last_pdu: bool,
}

/// Connection-wide outbound PDU intent, drained by the writer task.
/// Single writer serializes byte writes onto the wire so PDUs from
/// concurrent per-command tasks don't interleave at the byte level.
enum OutboundPdu {
    /// Solicit host data for a command. Emitted by a per-command task
    /// that needs R2T fulfillment.
    R2T {
        cccid: u16,
        ttag: u16,
        r2to: u32,
        r2tl: u32,
    },
    /// Command completion. The writer expands this to one of three
    /// wire shapes depending on the response: a single SUCCESS-folded
    /// C2HData PDU (data-bearing response with a no-payload success
    /// CQE), a C2HData PDU followed by a CapsuleResp (data-bearing
    /// response with a non-foldable CQE), or a lone CapsuleResp (no
    /// data-in payload). Centralizing the SUCCESS-bit decision in
    /// the writer keeps the optimization in one place and guarantees
    /// the CapsuleResp follows its matching C2HData with no
    /// intervening PDU.
    CommandResponse { cqe: Cqe, data_in: Vec<u8> },
    /// Fatal protocol violation. Writer emits and then exits, which
    /// closes the WriteHalf and tears the TCP connection down.
    TermReq { fes: u16 },
    /// Orderly shutdown after a final CapsuleResp has been queued
    /// (Disconnect path). Writer flushes and exits.
    Shutdown,
}

/// Per-connection routing table: CCCID → per-command H2CData sender.
/// Populated by the reader when it spawns a per-command task that
/// will need R2T fulfillment; removed by the per-command task on
/// exit. Reader looks up by CCCID when it sees an H2CData PDU on
/// the wire.
type CommandTable = Arc<Mutex<HashMap<u16, mpsc::Sender<H2CDataChunk>>>>;

async fn serve_connection<S>(
    mut stream: S,
    peer: std::net::SocketAddr,
    handler: Arc<dyn NvmeCommandHandler>,
    controller_regs: Arc<ControllerRegs>,
    tls_host_nqn: Option<String>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // State 1: Initialization — ICReq → ICResp.
    let icreq_pdu = pdu::RawPdu::read_async(&mut stream).await?;
    if icreq_pdu.header.pdu_type != pdu::PduType::ICReq {
        write_term_req(&mut stream, fes::PDU_SEQUENCE_ERROR).await?;
        anyhow::bail!(
            "first PDU was {:?}, expected ICReq",
            icreq_pdu.header.pdu_type
        );
    }
    let icreq = pdu::ICReq::read_from(&icreq_pdu.body[..pdu::ICReq::PAYLOAD_LEN])?;
    if icreq.pfv != 0 {
        write_term_req(&mut stream, fes::INVALID_PDU_HEADER_FIELD).await?;
        anyhow::bail!("unsupported ICReq.PFV={}", icreq.pfv);
    }
    tracing::debug!(
        peer = %peer,
        pfv = icreq.pfv,
        hpda = icreq.hpda,
        host_dgst = icreq.dgst,
        maxr2t = icreq.maxr2t,
        "nvme-tcp: ICReq accepted",
    );
    // MAXR2T per NVMe/TCP §3.6.1 — 0 means "the controller shall
    // treat as 1 outstanding R2T per command". MVP only ever issues
    // one R2T per command anyway, so this is purely a sanity floor.
    let host_maxr2t = icreq.maxr2t.max(1);
    let icresp = pdu::ICResp {
        pfv: 0,
        cpda: 0,
        dgst: 0, // negotiate digests off; MVP keeps the codec simple
        maxh2cdata: ADVERTISED_MAXH2CDATA,
    };
    stream.write_all(&icresp.to_pdu()).await?;
    stream.flush().await?;

    // State 2: Admission — Connect (first CapsuleCmd).
    let connect_pdu = pdu::RawPdu::read_async(&mut stream).await?;
    if connect_pdu.header.pdu_type != pdu::PduType::CapsuleCmd {
        write_term_req(&mut stream, fes::PDU_SEQUENCE_ERROR).await?;
        anyhow::bail!(
            "expected Connect (CapsuleCmd), got {:?}",
            connect_pdu.header.pdu_type
        );
    }
    let (sqe, data_out) = pdu::parse_capsule_cmd(&connect_pdu)?;
    if AdminOpcode::from_u8(sqe.opcode) != Some(AdminOpcode::Fabrics) {
        // Refuse anything before Connect succeeds. Per NVMe-oF the
        // host must Connect before any other admin / I/O command.
        let cqe = Cqe::failure(sqe.cid, 0, 0, StatusField::connect_invalid_parameters());
        stream.write_all(&pdu::build_capsule_resp_pdu(&cqe)).await?;
        anyhow::bail!("first command was not Admin Fabrics");
    }
    let fctype = nvme_base::fabrics::extract_fctype(&sqe);
    if fctype != Some(FabricsType::Connect) {
        let cqe = Cqe::failure(sqe.cid, 0, 0, StatusField::connect_invalid_parameters());
        stream.write_all(&pdu::build_capsule_resp_pdu(&cqe)).await?;
        anyhow::bail!("first Fabrics command was not Connect ({:?})", fctype);
    }
    // QID lives at CDW10[31:16] — RECFMT at CDW10[15:0] is the
    // "Connection record format" version; we accept only 0.
    let recfmt = (sqe.cdw10 & 0xFFFF) as u16;
    if recfmt != 0 {
        let cqe = Cqe::failure(sqe.cid, 0, 0, StatusField::connect_invalid_parameters());
        stream.write_all(&pdu::build_capsule_resp_pdu(&cqe)).await?;
        anyhow::bail!("unsupported Connect RECFMT={}", recfmt);
    }
    let qid = ((sqe.cdw10 >> 16) & 0xFFFF) as u16;
    let connect_data = match data_out {
        Some(d) if d.len() == ConnectData::WIRE_LEN => ConnectData::parse(d)?,
        _ => {
            let cqe = Cqe::failure(sqe.cid, 0, 0, StatusField::connect_invalid_parameters());
            stream.write_all(&pdu::build_capsule_resp_pdu(&cqe)).await?;
            anyhow::bail!("Connect Data wrong size or missing");
        }
    };
    if connect_data.subnqn != handler.subnqn() {
        tracing::warn!(
            peer = %peer,
            host_subnqn = %connect_data.subnqn,
            our_subnqn = %handler.subnqn(),
            "nvme-tcp: Connect SUBNQN mismatch - refusing",
        );
        let cqe = Cqe::failure(sqe.cid, 0, 0, StatusField::connect_invalid_parameters());
        stream.write_all(&pdu::build_capsule_resp_pdu(&cqe)).await?;
        anyhow::bail!("SUBNQN mismatch");
    }
    // Defense-in-depth (TLS-PSK only): the host NQN we admitted at
    // the TLS layer MUST match the HostNQN the host claims in
    // Connect. A mismatch means the host authenticated as one
    // identity but is now claiming another — refuse the session.
    if let Some(ref tls_nqn) = tls_host_nqn
        && tls_nqn != &connect_data.hostnqn
    {
        tracing::warn!(
            peer = %peer,
            tls_host_nqn = %tls_nqn,
            connect_host_nqn = %connect_data.hostnqn,
            "nvme-tcp: HostNQN mismatch between TLS PSK identity and Connect - refusing",
        );
        let cqe = Cqe::failure(sqe.cid, 0, 0, StatusField::connect_invalid_parameters());
        stream.write_all(&pdu::build_capsule_resp_pdu(&cqe)).await?;
        anyhow::bail!("HostNQN TLS/Connect mismatch");
    }
    let cntlid: u16 = 1;
    let dw0 = nvme_base::fabrics::connect_response_dw0(cntlid, false);
    let cqe = Cqe::success(sqe.cid, qid, 0, dw0);
    stream.write_all(&pdu::build_capsule_resp_pdu(&cqe)).await?;
    stream.flush().await?;
    tracing::info!(
        peer = %peer,
        host_nqn = %connect_data.hostnqn,
        qid,
        cntlid,
        "nvme-tcp: Connect succeeded",
    );

    // State 3: Steady state — split the stream and run a PDU-demuxer
    // (reader) + serializing writer + per-command tasks concurrently.
    // Per NVMe/TCP §3.5 the host may pipeline PDUs across commands on
    // the same I/O queue (each PDU carries a CCCID identifying its
    // parent command); Linux nvme_tcp exercises this on any concurrent
    // write workload. A sequential serve loop would tear the connection
    // down the moment a second CapsuleCmd arrives during the first
    // command's R2T fulfillment.
    let _ = host_maxr2t;
    let (read_half, write_half) = tokio::io::split(stream);
    let (outbound_tx, outbound_rx) = mpsc::channel::<OutboundPdu>(OUTBOUND_CAPACITY);
    let commands: CommandTable = Arc::new(Mutex::new(HashMap::new()));

    let mut writer = tokio::spawn(writer_task(write_half, outbound_rx, peer));
    let mut reader = tokio::spawn(reader_task(
        read_half,
        peer,
        Arc::clone(&handler),
        Arc::clone(&controller_regs),
        Arc::clone(&commands),
        outbound_tx,
        qid,
    ));

    // Two valid exit paths:
    //   1. Reader exits first (host EOF / fatal protocol violation /
    //      H2CTermReq). Wait for the writer to drain any in-flight
    //      per-command responses, then return.
    //   2. Writer exits first (Disconnect → Shutdown, or TermReq).
    //      The read half is no longer useful; abort the reader so we
    //      don't hold the connection open waiting for the next PDU.
    tokio::select! {
        res = &mut writer => {
            reader.abort();
            let _ = (&mut reader).await;
            if let Ok(Err(e)) = res {
                tracing::debug!(peer = %peer, error = %e, "nvme-tcp: writer task error");
            }
        }
        res = &mut reader => {
            let writer_res = (&mut writer).await;
            if let Ok(Err(e)) = res {
                tracing::debug!(peer = %peer, error = %e, "nvme-tcp: reader task error");
            }
            if let Ok(Err(e)) = writer_res {
                tracing::debug!(peer = %peer, error = %e, "nvme-tcp: writer task error");
            }
        }
    }
    // Drop every remaining per-command H2CData sender so any task
    // still blocked on `rx.recv()` (e.g. R2T issued but H2CTermReq
    // arrived before the host fulfilled it) unblocks and exits.
    commands.lock().await.clear();
    Ok(())
}

/// PDU demuxer — owns the read half of the connection, the per-CCCID
/// routing table, and the `pending_fused` slot. Spawns one async task
/// per CapsuleCmd; H2CData PDUs are forwarded to the matching per-
/// command task's mpsc::Receiver. Returns on host close (EOF /
/// H2CTermReq) or fatal protocol violation.
async fn reader_task<R>(
    mut read: R,
    peer: std::net::SocketAddr,
    handler: Arc<dyn NvmeCommandHandler>,
    controller_regs: Arc<ControllerRegs>,
    commands: CommandTable,
    outbound: mpsc::Sender<OutboundPdu>,
    qid: u16,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut pending_fused: Option<(Sqe, Vec<u8>)> = None;
    loop {
        let raw = match pdu::RawPdu::read_async(&mut read).await {
            Ok(p) => p,
            Err(pdu::error::PduError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                tracing::debug!(peer = %peer, "nvme-tcp: connection closed by peer");
                if let Some((compare_sqe, _)) = pending_fused.take() {
                    let cqe = Cqe::failure(
                        compare_sqe.cid,
                        0,
                        0,
                        StatusField::aborted_due_to_missing_fused(),
                    );
                    let _ = outbound
                        .send(OutboundPdu::CommandResponse {
                            cqe,
                            data_in: Vec::new(),
                        })
                        .await;
                }
                return Ok(());
            }
            Err(e) => {
                let _ = outbound
                    .send(OutboundPdu::TermReq {
                        fes: fes::INVALID_PDU_HEADER_FIELD,
                    })
                    .await;
                return Err(e.into());
            }
        };
        match raw.header.pdu_type {
            pdu::PduType::H2CTermReq => {
                tracing::info!(peer = %peer, "nvme-tcp: H2CTermReq from host, closing");
                return Ok(());
            }
            pdu::PduType::H2CData => {
                let h2c = match pdu::parse_h2cdata(&raw) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(peer = %peer, error = %e, "nvme-tcp: malformed H2CData");
                        let _ = outbound
                            .send(OutboundPdu::TermReq {
                                fes: fes::INVALID_PDU_HEADER_FIELD,
                            })
                            .await;
                        return Err(e.into());
                    }
                };
                let sender = {
                    let map = commands.lock().await;
                    map.get(&h2c.cccid).cloned()
                };
                match sender {
                    Some(tx) => {
                        let chunk = H2CDataChunk {
                            ttag: h2c.ttag,
                            datao: h2c.datao,
                            data: h2c.data.to_vec(),
                            last_pdu: h2c.last_pdu,
                        };
                        let _ = tx.send(chunk).await;
                    }
                    None => {
                        tracing::warn!(
                            peer = %peer,
                            cccid = h2c.cccid,
                            "nvme-tcp: H2CData for unknown CCCID",
                        );
                        let _ = outbound
                            .send(OutboundPdu::TermReq {
                                fes: fes::INVALID_PDU_HEADER_FIELD,
                            })
                            .await;
                        anyhow::bail!("H2CData for unknown CCCID {}", h2c.cccid);
                    }
                }
            }
            pdu::PduType::CapsuleCmd => {
                let (sqe, icd) = match pdu::parse_capsule_cmd(&raw) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(peer = %peer, error = %e, "nvme-tcp: malformed CapsuleCmd");
                        let _ = outbound
                            .send(OutboundPdu::TermReq {
                                fes: fes::INVALID_PDU_HEADER_FIELD,
                            })
                            .await;
                        return Err(e.into());
                    }
                };
                let icd_owned = icd.map(|s| s.to_vec()).unwrap_or_default();

                // Fabrics — Property Get/Set/Disconnect touch the
                // shared ControllerRegs. They have no R2T flow and no
                // data-in payload; spawn a lightweight task so the
                // reader doesn't block on Disconnect's Shutdown emit.
                if AdminOpcode::from_u8(sqe.opcode) == Some(AdminOpcode::Fabrics) {
                    if let Some((compare_sqe, _)) = pending_fused.take() {
                        let cqe = Cqe::failure(
                            compare_sqe.cid,
                            0,
                            0,
                            StatusField::aborted_due_to_missing_fused(),
                        );
                        let _ = outbound
                            .send(OutboundPdu::CommandResponse {
                                cqe,
                                data_in: Vec::new(),
                            })
                            .await;
                    }
                    let regs = Arc::clone(&controller_regs);
                    let outbound_clone = outbound.clone();
                    tokio::spawn(async move {
                        let (cqe, close) = compute_fabrics_response(&sqe, &regs, peer);
                        let _ = outbound_clone
                            .send(OutboundPdu::CommandResponse {
                                cqe,
                                data_in: Vec::new(),
                            })
                            .await;
                        if close {
                            let _ = outbound_clone.send(OutboundPdu::Shutdown).await;
                        }
                    });
                    continue;
                }

                let direction = data_direction(sqe.opcode);
                if direction == DataDirection::Bidirectional {
                    tracing::warn!(
                        peer = %peer,
                        opc = sqe.opcode,
                        "nvme-tcp: bidirectional commands not supported",
                    );
                    let _ = outbound
                        .send(OutboundPdu::TermReq {
                            fes: fes::INVALID_PDU_HEADER_FIELD,
                        })
                        .await;
                    anyhow::bail!("bidirectional opcode 0x{:02X}", sqe.opcode);
                }
                let sgl_len = pdu::sgl_data_length(&sqe);
                let icd_len = icd_owned.len() as u32;

                // Fused Compare+Write — first half is always fully
                // in-capsule (Compare's data accompanies the SQE; we
                // don't run R2T for the compare half). Stash and wait
                // for the matching second half.
                if sqe.fuse == Fuse::First {
                    if qid == 0 {
                        let cqe = Cqe::failure(sqe.cid, 0, 0, StatusField::invalid_field());
                        let _ = outbound
                            .send(OutboundPdu::CommandResponse {
                                cqe,
                                data_in: Vec::new(),
                            })
                            .await;
                        continue;
                    }
                    if let Some((prev_sqe, _)) = pending_fused.take() {
                        let cqe = Cqe::failure(
                            prev_sqe.cid,
                            0,
                            0,
                            StatusField::aborted_due_to_missing_fused(),
                        );
                        let _ = outbound
                            .send(OutboundPdu::CommandResponse {
                                cqe,
                                data_in: Vec::new(),
                            })
                            .await;
                    }
                    pending_fused = Some((sqe, icd_owned));
                    continue;
                }
                if sqe.fuse == Fuse::Second {
                    let Some((compare_sqe, compare_data)) = pending_fused.take() else {
                        let cqe = Cqe::failure(
                            sqe.cid,
                            0,
                            0,
                            StatusField::aborted_due_to_missing_fused(),
                        );
                        let _ = outbound
                            .send(OutboundPdu::CommandResponse {
                                cqe,
                                data_in: Vec::new(),
                            })
                            .await;
                        continue;
                    };
                    let handler_clone = Arc::clone(&handler);
                    let outbound_clone = outbound.clone();
                    let write_data = icd_owned;
                    tokio::spawn(async move {
                        let (cqe_c, cqe_w) = handler_clone
                            .handle_fused_compare_write(
                                IoCommand {
                                    sqe: compare_sqe,
                                    data_out: Some(&compare_data),
                                    data_in_max: u32::MAX,
                                },
                                IoCommand {
                                    sqe,
                                    data_out: Some(&write_data),
                                    data_in_max: u32::MAX,
                                },
                            )
                            .await;
                        let _ = outbound_clone
                            .send(OutboundPdu::CommandResponse {
                                cqe: cqe_c,
                                data_in: Vec::new(),
                            })
                            .await;
                        let _ = outbound_clone
                            .send(OutboundPdu::CommandResponse {
                                cqe: cqe_w,
                                data_in: Vec::new(),
                            })
                            .await;
                    });
                    continue;
                }

                // Non-fused command — a pending fused-first orphan
                // must abort before we start this one.
                if let Some((orphan_sqe, _)) = pending_fused.take() {
                    let cqe = Cqe::failure(
                        orphan_sqe.cid,
                        0,
                        0,
                        StatusField::aborted_due_to_missing_fused(),
                    );
                    let _ = outbound
                        .send(OutboundPdu::CommandResponse {
                            cqe,
                            data_in: Vec::new(),
                        })
                        .await;
                }

                // Decide R2T need; only R2T-needing commands consume
                // a slot in the per-connection inflight table. Over
                // cap → Namespace Not Ready, without spawning a task.
                let needs_r2t = direction == DataDirection::HostToController && sgl_len > icd_len;
                let h2c_rx = if needs_r2t {
                    let mut map = commands.lock().await;
                    if map.len() >= INFLIGHT_CAP {
                        let cqe = Cqe::failure(sqe.cid, 0, 0, StatusField::namespace_not_ready());
                        let _ = outbound
                            .send(OutboundPdu::CommandResponse {
                                cqe,
                                data_in: Vec::new(),
                            })
                            .await;
                        continue;
                    }
                    let (tx, rx) = mpsc::channel(PER_COMMAND_H2C_CAPACITY);
                    map.insert(sqe.cid, tx);
                    Some(rx)
                } else {
                    None
                };

                let handler_clone = Arc::clone(&handler);
                let outbound_clone = outbound.clone();
                let commands_clone = Arc::clone(&commands);
                tokio::spawn(handle_command(
                    sqe,
                    icd_owned,
                    sgl_len,
                    qid,
                    h2c_rx,
                    handler_clone,
                    outbound_clone,
                    commands_clone,
                    peer,
                ));
            }
            other => {
                tracing::warn!(
                    peer = %peer,
                    pdu_type = ?other,
                    "nvme-tcp: unexpected PDU in command loop",
                );
                let _ = outbound
                    .send(OutboundPdu::TermReq {
                        fes: fes::INVALID_PDU_HEADER_TYPE,
                    })
                    .await;
                anyhow::bail!("unexpected PDU type {:?} in command loop", other);
            }
        }
    }
}

/// Wire-serializing writer — owns the write half of the connection.
/// Drains the per-connection outbound mpsc and expands
/// `CommandResponse` into either one SUCCESS-folded C2HData PDU, one
/// C2HData + one CapsuleResp, or a single CapsuleResp. Centralizing
/// the SUCCESS-bit decision here keeps the optimization in one place
/// and guarantees the CapsuleResp follows its matching C2HData with
/// no other PDU interleaved.
///
/// Returns when the channel closes (every sender dropped) or after
/// emitting a terminal PDU (`TermReq` / `Shutdown`).
async fn writer_task<W>(
    mut write: W,
    mut outbound: mpsc::Receiver<OutboundPdu>,
    peer: std::net::SocketAddr,
) -> Result<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    while let Some(pdu) = outbound.recv().await {
        match pdu {
            OutboundPdu::R2T {
                cccid,
                ttag,
                r2to,
                r2tl,
            } => {
                write
                    .write_all(&pdu::build_r2t_pdu(cccid, ttag, r2to, r2tl))
                    .await?;
                write.flush().await?;
            }
            OutboundPdu::CommandResponse { cqe, data_in } => {
                if !data_in.is_empty() {
                    // SUCCESS-bit optimization (NVMe/TCP §3.6.7): if
                    // the CQE is a no-payload success, fold completion
                    // into the C2HData and skip the trailing
                    // CapsuleResp. One round-trip saved on every
                    // Identify / Get Log Page / NVM Read.
                    let can_fold =
                        cqe.status == StatusField::SUCCESS && cqe.dw0 == 0 && cqe.dw1 == 0;
                    let extra = if can_fold { pdu::C2H_FLAGS_SUCCESS } else { 0 };
                    write
                        .write_all(&pdu::build_c2hdata_pdu_with_flags(cqe.cid, &data_in, extra))
                        .await?;
                    if !can_fold {
                        write.write_all(&pdu::build_capsule_resp_pdu(&cqe)).await?;
                    }
                } else {
                    write.write_all(&pdu::build_capsule_resp_pdu(&cqe)).await?;
                }
                write.flush().await?;
            }
            OutboundPdu::TermReq { fes } => {
                let _ = write.write_all(&pdu::build_c2h_term_req_pdu(fes)).await;
                let _ = write.flush().await;
                tracing::debug!(peer = %peer, fes, "nvme-tcp: writer exiting after TermReq");
                return Ok(());
            }
            OutboundPdu::Shutdown => {
                // Half-close the write side — TcpStream sends FIN,
                // TlsStream sends close_notify. The host's read side
                // sees EOF, which is how it observes Disconnect taking
                // effect.
                let _ = write.shutdown().await;
                tracing::debug!(peer = %peer, "nvme-tcp: writer exiting after Shutdown");
                return Ok(());
            }
        }
    }
    Ok(())
}

/// One command's full lifecycle. For R2T-needing writes: emits R2T,
/// drains H2CData chunks from its per-command receiver into the
/// transfer buffer, then dispatches. For everything else: dispatches
/// immediately with whatever ICD (or none) arrived in the CapsuleCmd.
/// In all paths: emits a single `CommandResponse` and removes itself
/// from the inflight table.
async fn handle_command(
    sqe: Sqe,
    icd: Vec<u8>,
    sgl_len: u32,
    qid: u16,
    h2c_rx: Option<mpsc::Receiver<H2CDataChunk>>,
    handler: Arc<dyn NvmeCommandHandler>,
    outbound: mpsc::Sender<OutboundPdu>,
    commands: CommandTable,
    peer: std::net::SocketAddr,
) {
    let cccid = sqe.cid;
    let buf: Option<Vec<u8>> = if let Some(mut rx) = h2c_rx {
        const TTAG: u16 = 1;
        let icd_len = icd.len() as u32;
        let r2to = icd_len;
        let r2tl = sgl_len - icd_len;
        let mut assembled = vec![0u8; sgl_len as usize];
        if !icd.is_empty() {
            assembled[..icd.len()].copy_from_slice(&icd);
        }
        if outbound
            .send(OutboundPdu::R2T {
                cccid,
                ttag: TTAG,
                r2to,
                r2tl,
            })
            .await
            .is_err()
        {
            remove_from_table(&commands, cccid).await;
            return;
        }
        let r2t_end = (r2to as u64) + (r2tl as u64);
        let mut received: u32 = 0;
        while received < r2tl {
            let chunk = match rx.recv().await {
                Some(c) => c,
                None => {
                    // Reader is gone (connection tearing down) — drop.
                    remove_from_table(&commands, cccid).await;
                    return;
                }
            };
            if chunk.ttag != TTAG {
                tracing::warn!(
                    peer = %peer,
                    cccid,
                    got_ttag = chunk.ttag,
                    want_ttag = TTAG,
                    "nvme-tcp: H2CData TTAG mismatch",
                );
                let _ = outbound
                    .send(OutboundPdu::TermReq {
                        fes: fes::INVALID_PDU_HEADER_FIELD,
                    })
                    .await;
                remove_from_table(&commands, cccid).await;
                return;
            }
            let datao = chunk.datao;
            let datal = chunk.data.len() as u32;
            let Some(end_u64) = (datao as u64).checked_add(datal as u64) else {
                let _ = outbound
                    .send(OutboundPdu::TermReq {
                        fes: fes::INVALID_PDU_HEADER_FIELD,
                    })
                    .await;
                remove_from_table(&commands, cccid).await;
                return;
            };
            if (datao as u64) < (r2to as u64) || end_u64 > r2t_end {
                tracing::warn!(
                    peer = %peer,
                    cccid,
                    datao,
                    datal,
                    r2to,
                    r2t_end,
                    "nvme-tcp: H2CData outside R2T window",
                );
                let _ = outbound
                    .send(OutboundPdu::TermReq {
                        fes: fes::INVALID_PDU_HEADER_FIELD,
                    })
                    .await;
                remove_from_table(&commands, cccid).await;
                return;
            }
            let end_us = end_u64 as usize;
            assembled[datao as usize..end_us].copy_from_slice(&chunk.data);
            received = received.saturating_add(datal);
            if chunk.last_pdu {
                if received < r2tl {
                    tracing::warn!(
                        peer = %peer,
                        cccid,
                        received,
                        expected = r2tl,
                        "nvme-tcp: H2CData LAST_PDU set before transfer complete",
                    );
                    let _ = outbound
                        .send(OutboundPdu::TermReq {
                            fes: fes::INVALID_PDU_HEADER_FIELD,
                        })
                        .await;
                    remove_from_table(&commands, cccid).await;
                    return;
                }
                break;
            }
        }
        remove_from_table(&commands, cccid).await;
        Some(assembled)
    } else if matches!(data_direction(sqe.opcode), DataDirection::HostToController) {
        // Fully in-capsule (ICD covers the SGL length, or SGL = 0).
        Some(icd)
    } else {
        None
    };

    let data_out = buf.as_deref();
    let response = if qid == 0 {
        handler
            .handle_admin(AdminCommand {
                sqe,
                data_out,
                data_in_max: u32::MAX,
            })
            .await
    } else {
        handler
            .handle_io(IoCommand {
                sqe,
                data_out,
                data_in_max: u32::MAX,
            })
            .await
    };
    let _ = outbound
        .send(OutboundPdu::CommandResponse {
            cqe: response.cqe,
            data_in: response.data_in,
        })
        .await;
}

async fn remove_from_table(commands: &CommandTable, cccid: u16) {
    let mut map = commands.lock().await;
    map.remove(&cccid);
}

/// Compute the CQE for a post-Connect Fabrics command, plus a bool
/// indicating whether the connection should close (Disconnect).
/// Pure function over `Sqe` + `ControllerRegs`; spawned per-command
/// to keep the reader loop non-blocking.
fn compute_fabrics_response(
    sqe: &Sqe,
    regs: &Arc<ControllerRegs>,
    peer: std::net::SocketAddr,
) -> (Cqe, bool) {
    let fctype = nvme_base::fabrics::extract_fctype(sqe);
    match fctype {
        Some(FabricsType::PropertyGet) => {
            let attrib_8 = (sqe.cdw10 & 0b1) != 0;
            let offset = sqe.cdw11;
            match regs.property_get(offset, attrib_8) {
                Some(val) => {
                    let mut cqe = Cqe::success(sqe.cid, 0, 0, val as u32);
                    cqe.dw1 = (val >> 32) as u32;
                    tracing::debug!(
                        peer = %peer,
                        offset = format!("0x{:02X}", offset),
                        attrib_8,
                        value = format!("0x{:016X}", val),
                        "nvme-tcp: Property Get",
                    );
                    (cqe, false)
                }
                None => {
                    tracing::debug!(
                        peer = %peer,
                        offset = format!("0x{:02X}", offset),
                        attrib_8,
                        "nvme-tcp: Property Get refused (unknown offset / wrong width)",
                    );
                    (
                        Cqe::failure(sqe.cid, 0, 0, StatusField::invalid_field()),
                        false,
                    )
                }
            }
        }
        Some(FabricsType::PropertySet) => {
            let attrib_8 = (sqe.cdw10 & 0b1) != 0;
            let offset = sqe.cdw11;
            let value = u64::from(sqe.cdw12) | (u64::from(sqe.cdw13) << 32);
            if regs.property_set(offset, attrib_8, value).is_some() {
                tracing::debug!(
                    peer = %peer,
                    offset = format!("0x{:02X}", offset),
                    attrib_8,
                    value = format!("0x{:016X}", value),
                    csts = format!("0x{:08X}", regs.csts()),
                    "nvme-tcp: Property Set",
                );
                (Cqe::success(sqe.cid, 0, 0, 0), false)
            } else {
                (
                    Cqe::failure(sqe.cid, 0, 0, StatusField::invalid_field()),
                    false,
                )
            }
        }
        Some(FabricsType::Disconnect) => {
            tracing::info!(peer = %peer, "nvme-tcp: Disconnect - closing connection");
            (Cqe::success(sqe.cid, 0, 0, 0), true)
        }
        Some(FabricsType::Connect) => {
            tracing::warn!(peer = %peer, "nvme-tcp: refusing second Connect on established queue");
            (
                Cqe::failure(sqe.cid, 0, 0, StatusField::connect_invalid_parameters()),
                false,
            )
        }
        Some(other) => {
            tracing::warn!(peer = %peer, fctype = ?other, "nvme-tcp: unsupported Fabrics command");
            (
                Cqe::failure(sqe.cid, 0, 0, StatusField::invalid_field()),
                false,
            )
        }
        None => (
            Cqe::failure(sqe.cid, 0, 0, StatusField::invalid_field()),
            false,
        ),
    }
}

/// Best-effort C2HTermReq write before closing. Used by the State 1
/// / State 2 admission code (ICReq / Connect) — once State 3 has
/// split the stream, the writer task owns all PDU emission and
/// fatal errors travel as `OutboundPdu::TermReq` through the channel.
async fn write_term_req<S>(stream: &mut S, fes: u16) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let _ = stream.write_all(&pdu::build_c2h_term_req_pdu(fes)).await;
    let _ = stream.flush().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use nvme_base::AdminOpcode;
    use nvme_nvm::{NvmOpcode, NvmeResponse};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::{Duration, timeout};

    /// Stub handler. Returns canned data so the transport's loop
    /// can be exercised without pulling in core-block + shared-cloud.
    /// Tracks the number of admin / io calls + captures the most
    /// recent I/O `data_out` so tests can verify R2T-assembled
    /// payloads round-tripped intact.
    struct StubHandler {
        subnqn: String,
        admin_calls: AtomicU32,
        io_calls: AtomicU32,
        last_io_data: Mutex<Option<Vec<u8>>>,
    }

    impl StubHandler {
        fn new(subnqn: &str) -> Arc<Self> {
            Arc::new(Self {
                subnqn: subnqn.to_string(),
                admin_calls: AtomicU32::new(0),
                io_calls: AtomicU32::new(0),
                last_io_data: Mutex::new(None),
            })
        }
    }

    #[async_trait]
    impl NvmeCommandHandler for StubHandler {
        fn subnqn(&self) -> &str {
            &self.subnqn
        }
        async fn handle_admin(&self, cmd: AdminCommand<'_>) -> NvmeResponse {
            self.admin_calls.fetch_add(1, Ordering::SeqCst);
            if AdminOpcode::from_u8(cmd.sqe.opcode) == Some(AdminOpcode::Identify) {
                let mut data = vec![0u8; 4096];
                data[0] = 0xC0;
                NvmeResponse::with_data(Cqe::success(cmd.sqe.cid, 0, 0, 0), data)
            } else {
                NvmeResponse::just(Cqe::success(cmd.sqe.cid, 0, 0, 0))
            }
        }
        async fn handle_io(&self, cmd: IoCommand<'_>) -> NvmeResponse {
            self.io_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(d) = cmd.data_out {
                *self.last_io_data.lock().unwrap() = Some(d.to_vec());
            }
            if NvmOpcode::from_u8(cmd.sqe.opcode) == Some(NvmOpcode::Read) {
                NvmeResponse::with_data(Cqe::success(cmd.sqe.cid, 0, 0, 0), vec![0xAAu8; 4])
            } else {
                NvmeResponse::just(Cqe::success(cmd.sqe.cid, 0, 0, 0))
            }
        }
    }

    /// Spawn the server on `127.0.0.1:0`, return the bound port. Each
    /// test gets a fresh `ControllerRegs` so CC writes from one test
    /// don't leak into the next.
    async fn spawn_server(handler: Arc<StubHandler>) -> u16 {
        spawn_server_with_regs(handler, Arc::new(ControllerRegs::new())).await
    }

    async fn spawn_server_with_regs(handler: Arc<StubHandler>, regs: Arc<ControllerRegs>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = Arc::clone(&handler) as Arc<dyn NvmeCommandHandler>;
        tokio::spawn(async move {
            let _ = accept_loop(listener, h, regs, None).await;
        });
        port
    }

    /// Build an ICReq PDU.
    fn build_icreq_pdu() -> Vec<u8> {
        let mut buf = Vec::with_capacity(pdu::ICReq::PDU_LEN as usize);
        let header = pdu::CommonHeader {
            pdu_type: pdu::PduType::ICReq,
            flags: 0,
            hlen: (pdu::CommonHeader::WIRE_LEN + pdu::ICReq::PAYLOAD_LEN) as u8,
            pdo: 0,
            plen: pdu::ICReq::PDU_LEN,
        };
        header.write_to(&mut buf);
        let icreq = pdu::ICReq {
            pfv: 0,
            hpda: 0,
            dgst: 0,
            maxr2t: 16,
        };
        icreq.write_to(&mut buf);
        buf
    }

    /// Build a CapsuleCmd PDU carrying `sqe_bytes` (always 64) and
    /// optional in-capsule data.
    fn build_capsule_cmd_pdu(sqe_bytes: [u8; nvme_base::SQE_SIZE], data: &[u8]) -> Vec<u8> {
        const HLEN: u8 = (pdu::CommonHeader::WIRE_LEN + nvme_base::SQE_SIZE) as u8;
        let pdo = if data.is_empty() { 0 } else { HLEN };
        let plen = u32::from(HLEN) + data.len() as u32;
        let mut buf = Vec::with_capacity(plen as usize);
        let header = pdu::CommonHeader {
            pdu_type: pdu::PduType::CapsuleCmd,
            flags: 0,
            hlen: HLEN,
            pdo,
            plen,
        };
        header.write_to(&mut buf);
        buf.extend_from_slice(&sqe_bytes);
        buf.extend_from_slice(data);
        buf
    }

    /// Build a Connect CapsuleCmd, with our SUBNQN by default
    /// (override `subnqn` to test the mismatch case). NVMe-oF
    /// Fabrics SQEs put FCTYPE at byte 4 (overlapping NSID); QID
    /// lives at CDW10[31:16] with RECFMT in [15:0] (we always
    /// write 0).
    fn build_connect_pdu(subnqn: &str, qid: u16) -> Vec<u8> {
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = AdminOpcode::Fabrics as u8; // OPC
        // PSDT = SglInline (0b01) at CDW0[15:14].
        sqe[1] = 0b0100_0000;
        sqe[2..4].copy_from_slice(&0x42u16.to_le_bytes()); // CID
        sqe[4] = FabricsType::Connect as u8; // FCTYPE at NSID byte 0
        // CDW10: RECFMT=0 in low half, QID in high half.
        let cdw10 = u32::from(qid) << 16;
        sqe[40..44].copy_from_slice(&cdw10.to_le_bytes());
        let cd = ConnectData {
            hostid: [0xA1; 16],
            requested_cntlid: nvme_base::fabrics::CNTLID_ANY,
            subnqn: subnqn.to_string(),
            hostnqn: "nqn.2014-08.org.nvmexpress:uuid:test-host".to_string(),
        };
        let data = cd.to_bytes().unwrap();
        build_capsule_cmd_pdu(sqe, &data)
    }

    /// Build a Property Get fabrics command.
    fn build_property_get_pdu(cid: u16, offset: u32, attrib_8: bool) -> Vec<u8> {
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = AdminOpcode::Fabrics as u8;
        sqe[1] = 0b0100_0000;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4] = FabricsType::PropertyGet as u8;
        sqe[40] = if attrib_8 { 1 } else { 0 }; // ATTRIB at CDW10[2:0]
        sqe[44..48].copy_from_slice(&offset.to_le_bytes()); // CDW11
        build_capsule_cmd_pdu(sqe, &[])
    }

    /// Build a Property Set fabrics command. Value packed as
    /// CDW12 (lo) + CDW13 (hi).
    fn build_property_set_pdu(cid: u16, offset: u32, attrib_8: bool, value: u64) -> Vec<u8> {
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = AdminOpcode::Fabrics as u8;
        sqe[1] = 0b0100_0000;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4] = FabricsType::PropertySet as u8;
        sqe[40] = if attrib_8 { 1 } else { 0 };
        sqe[44..48].copy_from_slice(&offset.to_le_bytes());
        sqe[48..52].copy_from_slice(&(value as u32).to_le_bytes());
        sqe[52..56].copy_from_slice(&((value >> 32) as u32).to_le_bytes());
        build_capsule_cmd_pdu(sqe, &[])
    }

    /// Build a Disconnect fabrics command.
    fn build_disconnect_pdu(cid: u16) -> Vec<u8> {
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = AdminOpcode::Fabrics as u8;
        sqe[1] = 0b0100_0000;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4] = FabricsType::Disconnect as u8;
        build_capsule_cmd_pdu(sqe, &[])
    }

    async fn read_pdu_async(stream: &mut TcpStream) -> pdu::RawPdu {
        pdu::RawPdu::read_async(stream).await.unwrap()
    }

    #[tokio::test]
    async fn happy_path_handshake_and_admin_command() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

        // ICReq → ICResp
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        stream.flush().await.unwrap();
        let icresp_pdu = read_pdu_async(&mut stream).await;
        assert_eq!(icresp_pdu.header.pdu_type, pdu::PduType::ICResp);
        let icresp = pdu::ICResp::read_from(&icresp_pdu.body[..pdu::ICResp::PAYLOAD_LEN]).unwrap();
        assert_eq!(icresp.pfv, 0);
        assert_eq!(icresp.dgst, 0);

        // Connect
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 0))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let connect_resp = read_pdu_async(&mut stream).await;
        assert_eq!(connect_resp.header.pdu_type, pdu::PduType::CapsuleResp);
        // CQE at body[0..16]; CID at offset 12..14 in CQE.
        assert_eq!(&connect_resp.body[12..14], &0x42u16.to_le_bytes());
        // CNTLID returned in DW0[15:0] = 1
        let dw0 = u32::from_le_bytes([
            connect_resp.body[0],
            connect_resp.body[1],
            connect_resp.body[2],
            connect_resp.body[3],
        ]);
        assert_eq!(dw0 & 0xFFFF, 1);
        // Status word = 0 (success), P bit might be set by tx layer
        // — we don't set it. So zero.
        assert_eq!(&connect_resp.body[14..16], &0u16.to_le_bytes());

        // Admin Identify Controller
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = AdminOpcode::Identify as u8;
        sqe[2] = 0x77; // CID
        sqe[40] = nvme_base::identify::CNS::Controller as u8;
        stream
            .write_all(&build_capsule_cmd_pdu(sqe, &[]))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        // Expect a single C2HData PDU with the SUCCESS bit set —
        // the CQE folds into this PDU instead of a separate
        // CapsuleResp follow-up.
        let c2h = read_pdu_async(&mut stream).await;
        assert_eq!(c2h.header.pdu_type, pdu::PduType::C2HData);
        assert_eq!(c2h.body.len(), 16 + 4096);
        assert_eq!(c2h.body[16], 0xC0); // stub canary
        assert_eq!(&c2h.body[0..2], &0x77u16.to_le_bytes());
        assert_eq!(
            c2h.header.flags & pdu::C2H_FLAGS_SUCCESS,
            pdu::C2H_FLAGS_SUCCESS
        );
        assert_eq!(
            c2h.header.flags & pdu::C2H_FLAGS_LAST_PDU,
            pdu::C2H_FLAGS_LAST_PDU
        );
        assert_eq!(handler.admin_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn subnqn_mismatch_returns_invalid_parameters() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await; // ICResp

        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.wrong:subsys", 0))
            .await
            .unwrap();
        let resp = read_pdu_async(&mut stream).await;
        assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
        // Status word = SC=0x82 (Connect Invalid Parameters), SCT=1
        // (CommandSpecific), DNR=1. Packed: P at bit0 (0), SC<<1,
        // SCT<<9, DNR<<15.
        let expected = StatusField::connect_invalid_parameters().to_u16();
        let actual = u16::from_le_bytes([resp.body[14], resp.body[15]]);
        assert_eq!(actual, expected);
        assert_eq!(handler.admin_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unexpected_first_pdu_triggers_term_req() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        // Send a CapsuleCmd as the first PDU (illegal — must be ICReq).
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = 0x06;
        stream
            .write_all(&build_capsule_cmd_pdu(sqe, &[]))
            .await
            .unwrap();
        // Expect C2HTermReq then EOF
        let term = read_pdu_async(&mut stream).await;
        assert_eq!(term.header.pdu_type, pdu::PduType::C2HTermReq);
        // FES at body[0..2] = PDU_SEQUENCE_ERROR = 0x02
        assert_eq!(&term.body[0..2], &0x0002u16.to_le_bytes());

        // EOF expected next
        let mut tmp = [0u8; 1];
        let n = timeout(Duration::from_secs(2), stream.read(&mut tmp))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 0);
    }

    /// Build an H2CData PDU split with the given CCCID/TTAG carrying
    /// `data` at byte offset `datao`. `last` sets the LAST_PDU flag.
    fn build_h2cdata_pdu(cccid: u16, ttag: u16, datao: u32, data: &[u8], last: bool) -> Vec<u8> {
        const HLEN: u8 = 24;
        let plen = u32::from(HLEN) + data.len() as u32;
        let mut buf = Vec::with_capacity(plen as usize);
        let header = pdu::CommonHeader {
            pdu_type: pdu::PduType::H2CData,
            flags: if last { pdu::H2C_FLAGS_LAST_PDU } else { 0 },
            hlen: HLEN,
            pdo: HLEN,
            plen,
        };
        header.write_to(&mut buf);
        buf.extend_from_slice(&cccid.to_le_bytes());
        buf.extend_from_slice(&ttag.to_le_bytes());
        buf.extend_from_slice(&datao.to_le_bytes());
        buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&[0u8; 4]); // reserved
        buf.extend_from_slice(data);
        buf
    }

    /// Build a Write CapsuleCmd with no in-capsule data and an SGL
    /// length advertising `total_len` bytes — forces the server into
    /// the R2T fulfillment path.
    fn build_write_no_icd_pdu(cid: u16, nsid: u32, total_len: u32) -> Vec<u8> {
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = NvmOpcode::Write as u8;
        sqe[1] = 0b0100_0000; // PSDT = SglInline
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
        // SGL descriptor at DPTR (bytes 24..40 of SQE):
        //   bytes 0..8  address (zero for offset-based / not used)
        //   bytes 8..12 length
        //   byte 15     identifier (type | subtype)
        // Length field is what sgl_data_length reads.
        sqe[24 + 8..24 + 12].copy_from_slice(&total_len.to_le_bytes());
        build_capsule_cmd_pdu(sqe, &[])
    }

    #[tokio::test]
    async fn multi_pdu_write_round_trip_via_r2t() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await; // ICResp
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 1))
            .await
            .unwrap();
        let _ = read_pdu_async(&mut stream).await; // Connect resp

        // 1 KiB write split into two H2CData PDUs of 600 + 424 bytes.
        // Sizes are deliberately non-power-of-two and non-equal so the
        // offset / length accounting catches anything off-by-one.
        let total_len: u32 = 1024;
        let payload: Vec<u8> = (0..total_len).map(|i| (i & 0xFF) as u8).collect();
        let cid: u16 = 0x99;
        stream
            .write_all(&build_write_no_icd_pdu(cid, 1, total_len))
            .await
            .unwrap();
        stream.flush().await.unwrap();

        // Server emits an R2T requesting the whole transfer.
        let r2t = read_pdu_async(&mut stream).await;
        assert_eq!(r2t.header.pdu_type, pdu::PduType::R2T);
        assert_eq!(&r2t.body[0..2], &cid.to_le_bytes()); // CCCID
        let ttag = u16::from_le_bytes([r2t.body[2], r2t.body[3]]);
        let r2to = u32::from_le_bytes([r2t.body[4], r2t.body[5], r2t.body[6], r2t.body[7]]);
        let r2tl = u32::from_le_bytes([r2t.body[8], r2t.body[9], r2t.body[10], r2t.body[11]]);
        assert_eq!(r2to, 0);
        assert_eq!(r2tl, total_len);

        // Reply with two H2CData PDUs.
        stream
            .write_all(&build_h2cdata_pdu(cid, ttag, 0, &payload[..600], false))
            .await
            .unwrap();
        stream
            .write_all(&build_h2cdata_pdu(cid, ttag, 600, &payload[600..], true))
            .await
            .unwrap();
        stream.flush().await.unwrap();

        // CapsuleResp success — no data_in for a Write.
        let resp = read_pdu_async(&mut stream).await;
        assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
        assert_eq!(&resp.body[12..14], &cid.to_le_bytes());
        let status = u16::from_le_bytes([resp.body[14], resp.body[15]]);
        assert_eq!(status, 0, "Write should succeed");

        // Verify the assembled bytes reached the handler intact.
        let captured = handler
            .last_io_data
            .lock()
            .unwrap()
            .clone()
            .expect("handler should have received data_out");
        assert_eq!(captured.len(), total_len as usize);
        assert_eq!(captured, payload);
        assert_eq!(handler.io_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn write_with_partial_icd_uses_r2t_for_tail() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await;
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 1))
            .await
            .unwrap();
        let _ = read_pdu_async(&mut stream).await;

        // 2 KiB write: 800 bytes in capsule, 1248 via R2T.
        let total_len: u32 = 2048;
        let icd_len: usize = 800;
        let payload: Vec<u8> = (0..total_len).map(|i| (i & 0xFF) as u8).collect();

        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = NvmOpcode::Write as u8;
        sqe[1] = 0b0100_0000;
        sqe[2..4].copy_from_slice(&0x77u16.to_le_bytes());
        sqe[4..8].copy_from_slice(&1u32.to_le_bytes()); // NSID
        sqe[24 + 8..24 + 12].copy_from_slice(&total_len.to_le_bytes());
        let pdu_bytes = build_capsule_cmd_pdu(sqe, &payload[..icd_len]);
        stream.write_all(&pdu_bytes).await.unwrap();
        stream.flush().await.unwrap();

        // Server emits R2T for offset=800, length=1248.
        let r2t = read_pdu_async(&mut stream).await;
        assert_eq!(r2t.header.pdu_type, pdu::PduType::R2T);
        let ttag = u16::from_le_bytes([r2t.body[2], r2t.body[3]]);
        let r2to = u32::from_le_bytes([r2t.body[4], r2t.body[5], r2t.body[6], r2t.body[7]]);
        let r2tl = u32::from_le_bytes([r2t.body[8], r2t.body[9], r2t.body[10], r2t.body[11]]);
        assert_eq!(r2to, icd_len as u32);
        assert_eq!(r2tl, total_len - icd_len as u32);

        // Reply with one H2CData covering the full R2T window.
        stream
            .write_all(&build_h2cdata_pdu(
                0x77,
                ttag,
                icd_len as u32,
                &payload[icd_len..],
                true,
            ))
            .await
            .unwrap();
        stream.flush().await.unwrap();

        let resp = read_pdu_async(&mut stream).await;
        assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
        let status = u16::from_le_bytes([resp.body[14], resp.body[15]]);
        assert_eq!(status, 0);

        let captured = handler.last_io_data.lock().unwrap().clone().unwrap();
        assert_eq!(captured.len(), total_len as usize);
        assert_eq!(captured, payload, "ICD prefix + R2T tail mismatch");
    }

    #[tokio::test]
    async fn write_with_full_icd_skips_r2t() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await;
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 1))
            .await
            .unwrap();
        let _ = read_pdu_async(&mut stream).await;

        // Same 1 KiB payload, this time entirely in-capsule.
        let total_len: u32 = 1024;
        let payload: Vec<u8> = (0..total_len).map(|i| (0xFF - (i & 0xFF)) as u8).collect();
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = NvmOpcode::Write as u8;
        sqe[1] = 0b0100_0000;
        sqe[2..4].copy_from_slice(&0xAAu16.to_le_bytes()); // CID
        sqe[4..8].copy_from_slice(&1u32.to_le_bytes()); // NSID
        sqe[24 + 8..24 + 12].copy_from_slice(&total_len.to_le_bytes());
        let pdu_bytes = build_capsule_cmd_pdu(sqe, &payload);
        stream.write_all(&pdu_bytes).await.unwrap();
        stream.flush().await.unwrap();

        // Server must NOT emit an R2T — the ICD covers the SGL length.
        // Instead we should see CapsuleResp immediately.
        let resp = timeout(Duration::from_secs(2), read_pdu_async(&mut stream))
            .await
            .unwrap();
        assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
        let captured = handler
            .last_io_data
            .lock()
            .unwrap()
            .clone()
            .expect("handler should have received data_out");
        assert_eq!(captured, payload);
    }

    #[tokio::test]
    async fn h2cdata_cccid_mismatch_triggers_term_req() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await;
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 1))
            .await
            .unwrap();
        let _ = read_pdu_async(&mut stream).await;

        let total_len: u32 = 512;
        let cid: u16 = 0x55;
        stream
            .write_all(&build_write_no_icd_pdu(cid, 1, total_len))
            .await
            .unwrap();
        let r2t = read_pdu_async(&mut stream).await;
        let ttag = u16::from_le_bytes([r2t.body[2], r2t.body[3]]);

        // Send H2CData with WRONG CCCID — should yield C2HTermReq.
        let bogus_cid = cid.wrapping_add(1);
        stream
            .write_all(&build_h2cdata_pdu(
                bogus_cid,
                ttag,
                0,
                &vec![0u8; total_len as usize],
                true,
            ))
            .await
            .unwrap();
        let term = read_pdu_async(&mut stream).await;
        assert_eq!(term.header.pdu_type, pdu::PduType::C2HTermReq);
        assert_eq!(&term.body[0..2], &0x0001u16.to_le_bytes()); // INVALID_PDU_HEADER_FIELD

        let mut tmp = [0u8; 1];
        let n = timeout(Duration::from_secs(2), stream.read(&mut tmp))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(handler.io_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn property_get_cap_and_vs_then_set_cc_enables_controller() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let regs = Arc::new(ControllerRegs::new());
        let port = spawn_server_with_regs(Arc::clone(&handler), Arc::clone(&regs)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await;
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 0))
            .await
            .unwrap();
        let _ = read_pdu_async(&mut stream).await;

        // Property Get CAP (8 byte). Expect DW0|DW1 = CONTROLLER_CAP.
        stream
            .write_all(&build_property_get_pdu(
                0x10,
                nvme_base::fabrics::props::OFFSET_CAP,
                true,
            ))
            .await
            .unwrap();
        let resp = read_pdu_async(&mut stream).await;
        assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
        let dw0 = u32::from_le_bytes([resp.body[0], resp.body[1], resp.body[2], resp.body[3]]);
        let dw1 = u32::from_le_bytes([resp.body[4], resp.body[5], resp.body[6], resp.body[7]]);
        let combined = u64::from(dw0) | (u64::from(dw1) << 32);
        assert_eq!(combined, nvme_base::fabrics::CONTROLLER_CAP);

        // Property Get VS (4 byte). Expect 0x0001_0400.
        stream
            .write_all(&build_property_get_pdu(
                0x11,
                nvme_base::fabrics::props::OFFSET_VS,
                false,
            ))
            .await
            .unwrap();
        let resp = read_pdu_async(&mut stream).await;
        let dw0 = u32::from_le_bytes([resp.body[0], resp.body[1], resp.body[2], resp.body[3]]);
        assert_eq!(dw0, 0x0001_0400);

        // Property Set CC.EN=1. Verify CSTS.RDY flips.
        stream
            .write_all(&build_property_set_pdu(
                0x12,
                nvme_base::fabrics::props::OFFSET_CC,
                false,
                0x0046_0001, // EN=1, IOSQES=6, IOCQES=4
            ))
            .await
            .unwrap();
        let resp = read_pdu_async(&mut stream).await;
        let status = u16::from_le_bytes([resp.body[14], resp.body[15]]);
        assert_eq!(status, 0);
        assert_eq!(regs.cc(), 0x0046_0001);
        assert_eq!(regs.csts() & 1, 1, "RDY should flip after CC.EN=1");

        // Property Get CSTS confirms what the host now sees.
        stream
            .write_all(&build_property_get_pdu(
                0x13,
                nvme_base::fabrics::props::OFFSET_CSTS,
                false,
            ))
            .await
            .unwrap();
        let resp = read_pdu_async(&mut stream).await;
        let dw0 = u32::from_le_bytes([resp.body[0], resp.body[1], resp.body[2], resp.body[3]]);
        assert_eq!(dw0 & 1, 1);
    }

    #[tokio::test]
    async fn disconnect_returns_success_then_closes() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await;
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 0))
            .await
            .unwrap();
        let _ = read_pdu_async(&mut stream).await;

        stream.write_all(&build_disconnect_pdu(0xDD)).await.unwrap();
        let resp = read_pdu_async(&mut stream).await;
        assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
        let status = u16::from_le_bytes([resp.body[14], resp.body[15]]);
        assert_eq!(status, 0);

        // Connection should close cleanly after Disconnect.
        let mut tmp = [0u8; 1];
        let n = timeout(Duration::from_secs(2), stream.read(&mut tmp))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 0, "server should close after Disconnect");
    }

    #[tokio::test]
    async fn property_get_unknown_offset_returns_invalid_field() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await;
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 0))
            .await
            .unwrap();
        let _ = read_pdu_async(&mut stream).await;

        stream
            .write_all(&build_property_get_pdu(0x99, 0xDEAD_BEEF, false))
            .await
            .unwrap();
        let resp = read_pdu_async(&mut stream).await;
        let expected = StatusField::invalid_field().to_u16();
        let actual = u16::from_le_bytes([resp.body[14], resp.body[15]]);
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn io_queue_routes_through_handle_io() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await;
        // Connect on QID=1 (I/O queue).
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 1))
            .await
            .unwrap();
        let _ = read_pdu_async(&mut stream).await;

        // Send an NVM Read; stub returns 4 bytes 0xAA.
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = NvmOpcode::Read as u8;
        sqe[2] = 0x88; // CID
        sqe[4] = 0x01; // NSID
        stream
            .write_all(&build_capsule_cmd_pdu(sqe, &[]))
            .await
            .unwrap();
        let c2h = read_pdu_async(&mut stream).await;
        assert_eq!(c2h.header.pdu_type, pdu::PduType::C2HData);
        assert_eq!(&c2h.body[16..20], &[0xAA, 0xAA, 0xAA, 0xAA]);
        // SUCCESS-bit folded completion — no trailing CapsuleResp.
        assert_eq!(
            c2h.header.flags & pdu::C2H_FLAGS_SUCCESS,
            pdu::C2H_FLAGS_SUCCESS
        );
        assert_eq!(handler.io_calls.load(Ordering::SeqCst), 1);
        assert_eq!(handler.admin_calls.load(Ordering::SeqCst), 0);
    }

    /// Build an H2CTermReq PDU. Used by the
    /// `graceful_shutdown_with_pending_commands` test to assert
    /// in-flight R2T-fulfillment tasks unblock cleanly when the host
    /// hangs up mid-transfer.
    fn build_h2c_term_req_pdu() -> Vec<u8> {
        const HLEN: u8 = (pdu::CommonHeader::WIRE_LEN + 24) as u8;
        let plen = u32::from(HLEN);
        let mut buf = Vec::with_capacity(plen as usize);
        let header = pdu::CommonHeader {
            pdu_type: pdu::PduType::H2CTermReq,
            flags: 0,
            hlen: HLEN,
            pdo: 0,
            plen,
        };
        header.write_to(&mut buf);
        buf.extend_from_slice(&[0u8; 24]); // FES + FEI + reserved + rejected header
        buf
    }

    /// Two concurrent Writes on the same I/O queue. Both should
    /// produce R2T → fulfillment → CapsuleResp independently. CQEs
    /// may complete in any order — per-task spawning gives no
    /// per-CCCID ordering guarantee, which matches the NVMe spec.
    #[tokio::test]
    async fn two_concurrent_writes_complete_independently() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await;
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 1))
            .await
            .unwrap();
        let _ = read_pdu_async(&mut stream).await;

        let total_len: u32 = 512;
        let cid_a: u16 = 0x00A1;
        let cid_b: u16 = 0x00B2;
        let payload_a: Vec<u8> = (0..total_len).map(|i| (i & 0xFF) as u8).collect();
        let payload_b: Vec<u8> = (0..total_len)
            .map(|i| ((0xFF - (i & 0xFF)) & 0xFF) as u8)
            .collect();

        stream
            .write_all(&build_write_no_icd_pdu(cid_a, 1, total_len))
            .await
            .unwrap();
        stream
            .write_all(&build_write_no_icd_pdu(cid_b, 1, total_len))
            .await
            .unwrap();
        stream.flush().await.unwrap();

        let mut ttag_a = 0u16;
        let mut ttag_b = 0u16;
        for _ in 0..2 {
            let r2t = read_pdu_async(&mut stream).await;
            assert_eq!(r2t.header.pdu_type, pdu::PduType::R2T);
            let cccid = u16::from_le_bytes([r2t.body[0], r2t.body[1]]);
            let ttag = u16::from_le_bytes([r2t.body[2], r2t.body[3]]);
            if cccid == cid_a {
                ttag_a = ttag;
            } else if cccid == cid_b {
                ttag_b = ttag;
            } else {
                panic!("unexpected R2T CCCID 0x{:04X}", cccid);
            }
        }

        // Fulfill in reverse arrival order to prove there's no
        // CCCID-ordering coupling between R2T and H2CData.
        stream
            .write_all(&build_h2cdata_pdu(cid_b, ttag_b, 0, &payload_b, true))
            .await
            .unwrap();
        stream
            .write_all(&build_h2cdata_pdu(cid_a, ttag_a, 0, &payload_a, true))
            .await
            .unwrap();
        stream.flush().await.unwrap();

        let mut got_a = false;
        let mut got_b = false;
        for _ in 0..2 {
            let resp = read_pdu_async(&mut stream).await;
            assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
            let cid = u16::from_le_bytes([resp.body[12], resp.body[13]]);
            let status = u16::from_le_bytes([resp.body[14], resp.body[15]]);
            assert_eq!(status, 0, "Write CQE should be success");
            if cid == cid_a {
                got_a = true;
            } else if cid == cid_b {
                got_b = true;
            }
        }
        assert!(got_a && got_b, "both completions must arrive");
        assert_eq!(handler.io_calls.load(Ordering::SeqCst), 2);
    }

    /// The exact pattern Linux nvme_tcp emits on `mkfs.ext4` and any
    /// other concurrent-write workload: command A is mid-R2T-
    /// fulfillment when command B arrives. The old sequential server
    /// trips PDU_SEQUENCE_ERROR here and tears the connection down
    /// within milliseconds; the new demuxer + per-command-task model
    /// accepts B, issues its R2T, and only then waits for either
    /// fulfillment.
    #[tokio::test]
    async fn command_b_arrives_during_command_a_r2t() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await;
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 1))
            .await
            .unwrap();
        let _ = read_pdu_async(&mut stream).await;

        let total_len: u32 = 256;
        let cid_a: u16 = 0x00A1;
        let cid_b: u16 = 0x00B2;
        let payload_a: Vec<u8> = (0..total_len).map(|i| (i & 0xFF) as u8).collect();
        let payload_b: Vec<u8> = (0..total_len).map(|i| ((i * 3) & 0xFF) as u8).collect();

        stream
            .write_all(&build_write_no_icd_pdu(cid_a, 1, total_len))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let r2t_a = read_pdu_async(&mut stream).await;
        assert_eq!(r2t_a.header.pdu_type, pdu::PduType::R2T);
        assert_eq!(&r2t_a.body[0..2], &cid_a.to_le_bytes());
        let ttag_a = u16::from_le_bytes([r2t_a.body[2], r2t_a.body[3]]);

        // Send B without fulfilling A — the regression trigger.
        stream
            .write_all(&build_write_no_icd_pdu(cid_b, 1, total_len))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let r2t_b = read_pdu_async(&mut stream).await;
        assert_eq!(r2t_b.header.pdu_type, pdu::PduType::R2T);
        assert_eq!(&r2t_b.body[0..2], &cid_b.to_le_bytes());
        let ttag_b = u16::from_le_bytes([r2t_b.body[2], r2t_b.body[3]]);

        // Fulfill A.
        stream
            .write_all(&build_h2cdata_pdu(cid_a, ttag_a, 0, &payload_a, true))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let resp_a = read_pdu_async(&mut stream).await;
        assert_eq!(resp_a.header.pdu_type, pdu::PduType::CapsuleResp);
        assert_eq!(&resp_a.body[12..14], &cid_a.to_le_bytes());

        // Fulfill B.
        stream
            .write_all(&build_h2cdata_pdu(cid_b, ttag_b, 0, &payload_b, true))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let resp_b = read_pdu_async(&mut stream).await;
        assert_eq!(resp_b.header.pdu_type, pdu::PduType::CapsuleResp);
        assert_eq!(&resp_b.body[12..14], &cid_b.to_le_bytes());

        assert_eq!(handler.io_calls.load(Ordering::SeqCst), 2);
    }

    /// Fused Compare+Write requires the second-half SQE to be the
    /// immediate next command on the queue (NVM Command Set §3.2.5).
    /// When something else (here a Flush) arrives between the halves,
    /// the orphaned Compare gets `aborted_due_to_missing_fused` and
    /// the unrelated command proceeds normally.
    #[tokio::test]
    async fn fused_with_unrelated_command_between_halves() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await;
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 1))
            .await
            .unwrap();
        let _ = read_pdu_async(&mut stream).await;

        // Fused-first Compare with 8 bytes of comparison data inline.
        let cmp_cid: u16 = 0x00CC;
        let cmp_data = vec![0x11u8; 8];
        let mut cmp_sqe = [0u8; nvme_base::SQE_SIZE];
        cmp_sqe[0] = NvmOpcode::Compare as u8;
        // PSDT=SglInline (bits 7:6 of byte 1) + Fuse=First (bits 1:0).
        cmp_sqe[1] = 0b0100_0001;
        cmp_sqe[2..4].copy_from_slice(&cmp_cid.to_le_bytes());
        cmp_sqe[4..8].copy_from_slice(&1u32.to_le_bytes());
        cmp_sqe[24 + 8..24 + 12].copy_from_slice(&(cmp_data.len() as u32).to_le_bytes());
        stream
            .write_all(&build_capsule_cmd_pdu(cmp_sqe, &cmp_data))
            .await
            .unwrap();

        // Unrelated Flush (no data, no R2T).
        let flush_cid: u16 = 0x00FF;
        let mut flush_sqe = [0u8; nvme_base::SQE_SIZE];
        flush_sqe[0] = NvmOpcode::Flush as u8;
        flush_sqe[1] = 0b0100_0000; // PSDT=SglInline, FUSE=Normal
        flush_sqe[2..4].copy_from_slice(&flush_cid.to_le_bytes());
        flush_sqe[4..8].copy_from_slice(&1u32.to_le_bytes());
        stream
            .write_all(&build_capsule_cmd_pdu(flush_sqe, &[]))
            .await
            .unwrap();
        stream.flush().await.unwrap();

        let mut got_aborted = false;
        let mut got_flush = false;
        for _ in 0..2 {
            let resp = read_pdu_async(&mut stream).await;
            assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
            let cid = u16::from_le_bytes([resp.body[12], resp.body[13]]);
            let status = u16::from_le_bytes([resp.body[14], resp.body[15]]);
            if cid == cmp_cid {
                assert_eq!(
                    status,
                    StatusField::aborted_due_to_missing_fused().to_u16(),
                    "orphan Compare should be aborted",
                );
                got_aborted = true;
            } else if cid == flush_cid {
                assert_eq!(status, 0, "Flush should succeed");
                got_flush = true;
            } else {
                panic!("unexpected CID 0x{:04X}", cid);
            }
        }
        assert!(got_aborted && got_flush);
    }

    /// H2CTermReq while a per-command task is mid-R2T-fulfillment
    /// must unblock the task cleanly so the runtime doesn't leak it.
    /// The connection cleanup drops the H2CData sender, which closes
    /// the per-command receiver — the task drops on the next recv().
    /// Test passes if nothing panics.
    #[tokio::test]
    async fn graceful_shutdown_with_pending_commands() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await;
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 1))
            .await
            .unwrap();
        let _ = read_pdu_async(&mut stream).await;

        let total_len: u32 = 1024;
        let cid: u16 = 0x0042;
        stream
            .write_all(&build_write_no_icd_pdu(cid, 1, total_len))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let r2t = read_pdu_async(&mut stream).await;
        assert_eq!(r2t.header.pdu_type, pdu::PduType::R2T);

        // Send H2CTermReq instead of fulfilling the R2T. Server's
        // reader exits cleanly; cleanup drops the per-command H2CData
        // sender; the per-command task's rx.recv() returns None and
        // it exits.
        stream.write_all(&build_h2c_term_req_pdu()).await.unwrap();
        stream.flush().await.unwrap();
        drop(stream);
        // Give the runtime a tick to drain.
        tokio::time::sleep(Duration::from_millis(100)).await;
        // No panic = pass. Handler may or may not have been invoked
        // depending on the race between cleanup and dispatch; that's
        // not the property under test.
    }

    /// Over the per-connection inflight cap, the reader rejects new
    /// R2T-needing commands with `Namespace Not Ready` without
    /// spawning a task or emitting an R2T. Existing in-flight
    /// commands continue normally. Cap is `INFLIGHT_CAP` (256) so
    /// the test submits 257 commands and expects exactly one
    /// `Namespace Not Ready` plus 256 R2Ts back.
    #[tokio::test]
    async fn inflight_cap_returns_namespace_not_ready() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await;
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 1))
            .await
            .unwrap();
        let _ = read_pdu_async(&mut stream).await;

        let total_cmds: u16 = (INFLIGHT_CAP as u16) + 1;
        // Avoid CID 0 just to make any failure log readable.
        for i in 0..total_cmds {
            let cid = i + 1;
            stream
                .write_all(&build_write_no_icd_pdu(cid, 1, 64))
                .await
                .unwrap();
        }
        stream.flush().await.unwrap();

        let mut r2t_count: u32 = 0;
        let mut not_ready_count: u32 = 0;
        let expected_status = StatusField::namespace_not_ready().to_u16();
        for _ in 0..total_cmds {
            let raw = timeout(Duration::from_secs(5), read_pdu_async(&mut stream))
                .await
                .expect("server stalled reading inflight-cap responses");
            match raw.header.pdu_type {
                pdu::PduType::R2T => r2t_count += 1,
                pdu::PduType::CapsuleResp => {
                    let status = u16::from_le_bytes([raw.body[14], raw.body[15]]);
                    assert_eq!(
                        status, expected_status,
                        "over-cap CQE must be Namespace Not Ready",
                    );
                    not_ready_count += 1;
                }
                other => panic!("unexpected PDU type {:?}", other),
            }
        }
        assert_eq!(r2t_count, INFLIGHT_CAP as u32);
        assert_eq!(not_ready_count, 1);
    }
}
