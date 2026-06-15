// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVMe/TCP per-connection state machine.
//!
//! Three phases, in order:
//!
//! 1. **ICReq → ICResp** (NVMe/TCP §3.6.1 / §3.6.2). The very first
//!    PDU on every accepted TCP connection MUST be ICReq. We advertise
//!    an `MAXH2CDATA` of 128 KiB and honor whichever header- / data-
//!    digests (CRC32C) the host requests — a host sending `dgst=0`
//!    keeps the no-digest fast path; one configured for digests gets
//!    fully digested framing both ways (issue #78). Writes that exceed
//!    the in-capsule data budget fall back to R2T (see phase 3).
//! 2. **Connect** (NVMe-oF §6.3.1). The first CapsuleCmd carries an
//!    Admin Fabrics command with FCTYPE=0x01. We validate the
//!    host's SUBNQN against our subsystem's NQN and capture QID from
//!    CDW10[31:16]. QID=0 (admin queue) creates a controller and is
//!    assigned a fresh CNTLID; QID>0 (I/O queue) attaches to the
//!    controller named in Connect Data CNTLID. The assigned CNTLID is
//!    echoed in the Connect Response DW0.
//! 3. **Command loop**. Each subsequent CapsuleCmd routes through
//!    [`NvmeCommandHandler::handle_admin`] or
//!    [`NvmeCommandHandler::handle_io`] based on the captured QID. A
//!    write whose data doesn't fit the in-capsule budget is gathered
//!    via a single outstanding R2T (partial-ICD + R2T-tail stitching);
//!    a fused Compare+Write pair is tracked across its two SQEs.
//!    Responses with `data_in.len() > 0` get a preceding C2HData PDU
//!    (the CapsuleResp SUCCESS bit folded onto the last one where
//!    allowed); every other command gets a CapsuleResp.
//!
//! Property Get / Set (against the shared `ControllerRegs`),
//! Disconnect, and DH-HMAC-CHAP Authentication Send / Receive are
//! handled in the same loop. Errors on the wire (unexpected PDU type,
//! malformed Connect Data, SUBNQN mismatch, digest mismatch) are
//! reported via C2HTermReq with the appropriate Fatal Error Status
//! (FES) and the connection is dropped. Clean host-driven teardown
//! (H2CTermReq) closes silently.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, oneshot};

use nvme_base::{
    AdminOpcode, ConnectData, ControllerRegs, Cqe, FabricsType, Fuse, Sqe, StatusField,
};
use nvme_nvm::{AdminCommand, ConnToken, ControllerRegistry, IoCommand, NvmeCommandHandler};

use crate::auth as dhcrypt;
use crate::pdu;
use crate::tls::{NvmePskAcceptor, parse_psk_identity};
use nvme_base::auth as dhwire;

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
    /// Per-subsystem controller registry + AER hub, shared (one `Arc`)
    /// with the [`NvmeNvmDispatcher`] handler. The transport allocates
    /// CNTLIDs at Fabrics Connect and parks/delivers AERs; the handler
    /// *produces* reservation events (a reservation op on an I/O queue)
    /// the transport *consumes*. Same construct-once-at-boot pattern as
    /// `controller_regs`.
    pub aer: Arc<ControllerRegistry>,
    /// Optional TLS 1.3 PSK acceptor (NVMe-TCP §3.6.1.5). When set,
    /// every accepted TCP connection is wrapped in TLS before the
    /// NVMe ICReq/Connect handshake runs. Built by
    /// [`crate::tls::build_psk_acceptor`] from the daemon-loaded
    /// PSK table.
    pub tls: Option<NvmePskAcceptor>,
    /// Path to `nvmetcp-psks.json`. When set, the post-Connect path
    /// looks up the host's `volumes` admission set and threads it
    /// into every dispatched I/O / admin command (mirror of iSCSI
    /// CHAP-user → volume-set admission). `None` = no admission
    /// (see-everything) — used by tests and the in-process smoke
    /// path.
    pub psks_path: Option<std::path::PathBuf>,
    /// Path to `nvmetcp-dhchap.json`. When set, DH-HMAC-CHAP in-band
    /// authentication is required: every Connect Response asserts
    /// AUTHREQ and the host must complete the Authentication
    /// Send/Receive exchange before any other command. On success the
    /// host's `volumes` admission set comes from its dhchap entry
    /// (taking the place of the `psks_path` lookup). `None` = no
    /// in-band auth. Orthogonal to `tls`: setting both runs
    /// DH-HMAC-CHAP inside a TLS-PSK channel ("dhchap+tls").
    pub dhchap_path: Option<std::path::PathBuf>,
    /// Login-phase audit sink for DH-HMAC-CHAP success / failure
    /// (NVMe counterpart of shared-iscsi's CHAP audit hook). thurvsad
    /// wires its `AuditChannel` + the `shared_alerting` brute-force
    /// hook in here; tests and the in-process path pass
    /// [`NoopLoginAudit`]. The transport never stores secrets — only
    /// the metadata fields on [`LoginAuditEvent`].
    pub audit: Arc<dyn LoginAuditSink>,
}

// ===== Login audit hook =====

/// DH-HMAC-CHAP login-phase events the consuming product can opt into
/// auditing. Mirror of shared-iscsi's `LoginAuditEvent`: thurvsad
/// implements the sink against its `AuditChannel` (one
/// `nvmetcp.dhchap.{success,failure}` row per event) and the
/// `shared_alerting::record::chap_failure` brute-force counter;
/// everything else passes [`NoopLoginAudit`]. The transport decides
/// *which* wire outcomes count as a refused auth (negotiation
/// failure, reply mismatch, mutual-auth rejection, timeout); the sink
/// decides what to do with the row.
pub enum LoginAuditEvent<'a> {
    DhchapSuccess {
        peer: &'a str,
        host_nqn: &'a str,
        admitted_volumes: usize,
    },
    DhchapFailure {
        peer: &'a str,
        host_nqn: &'a str,
        /// Stable machine-readable category — one of
        /// `negotiation_failed`, `reply_invalid`,
        /// `controller_rejected`, `success2_tid_mismatch`, `timeout`.
        reason: &'a str,
        /// Human-readable detail (the error that closed the
        /// connection). Recorded in the audit row's `result`.
        error: String,
    },
}

/// Optional audit sink for DH-HMAC-CHAP login-phase events. The
/// transport never stores credentials — only the metadata fields on
/// [`LoginAuditEvent`].
pub trait LoginAuditSink: Send + Sync {
    fn record(&self, event: LoginAuditEvent<'_>);
}

/// Default no-op audit sink (tests; the in-process smoke path;
/// discovery controllers and other paths that don't audit logins).
#[derive(Default, Clone, Copy)]
pub struct NoopLoginAudit;

impl LoginAuditSink for NoopLoginAudit {
    fn record(&self, _event: LoginAuditEvent<'_>) {}
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
        config.aer,
        config.tls.map(Arc::new),
        config.psks_path,
        config.dhchap_path,
        config.audit,
    )
    .await
}

/// Accept-loop body factored out so tests can supply their own
/// pre-bound listener (e.g. `127.0.0.1:0` to let the kernel pick a
/// free port).
#[allow(clippy::too_many_arguments)]
pub async fn accept_loop(
    listener: TcpListener,
    handler: Arc<dyn NvmeCommandHandler>,
    controller_regs: Arc<ControllerRegs>,
    aer: Arc<ControllerRegistry>,
    tls: Option<Arc<NvmePskAcceptor>>,
    psks_path: Option<std::path::PathBuf>,
    dhchap_path: Option<std::path::PathBuf>,
    audit: Arc<dyn LoginAuditSink>,
) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        // NVMe/TCP is a latency-sensitive request/response protocol that
        // emits many small PDUs (24-byte R2T / CapsuleResp, sub-100-byte
        // auth messages). Disable Nagle so a small write following
        // unacked data isn't held for the peer's delayed-ACK timer
        // (~40 ms per affected exchange), e.g. R2T -> H2CData -> CapsuleResp
        // or the multi-message DH-HMAC-CHAP handshake (issue #243).
        if let Err(e) = stream.set_nodelay(true) {
            tracing::warn!(%peer, error = %e, "nvme/tcp: set_nodelay failed");
        }
        // Local address this connection landed on, captured before any
        // TLS wrap. The Discovery controller reflects its IP into the
        // Discovery Log Page TRADDR when the I/O listener is bound to a
        // wildcard address (the I/O dispatcher ignores it).
        let local_addr = stream.local_addr().ok();
        let handler = Arc::clone(&handler);
        let regs = Arc::clone(&controller_regs);
        let aer = Arc::clone(&aer);
        let tls = tls.clone();
        let psks_path = psks_path.clone();
        let dhchap_path = dhchap_path.clone();
        let audit = Arc::clone(&audit);
        tokio::spawn(async move {
            let result = match tls {
                None => {
                    serve_connection(
                        stream,
                        peer,
                        local_addr,
                        handler,
                        regs,
                        aer,
                        None,
                        psks_path,
                        dhchap_path,
                        audit,
                    )
                    .await
                }
                Some(acceptor) => match acceptor.accept(stream).await {
                    Ok(tls_stream) => {
                        // Capture the PSK identity the host used so
                        // serve_connection can cross-check it against
                        // the Connect command's HostNQN.
                        let tls_host_nqn = extract_negotiated_host_nqn(&tls_stream, peer);
                        serve_connection(
                            tls_stream,
                            peer,
                            local_addr,
                            handler,
                            regs,
                            aer,
                            tls_host_nqn,
                            psks_path,
                            dhchap_path,
                            audit,
                        )
                        .await
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

/// Hard ceiling on a single command's total transfer length (the SGL
/// data length in the SQE). Without it, a host's declared length flows
/// straight into a `vec![0u8; sgl_len]` allocation — up to 4 GiB from
/// one CapsuleCmd, a memory-amplification DoS. We advertise this as
/// MDTS in Identify Controller (`MDTS = log2(MAX_TRANSFER_BYTES / 4 KiB)`,
/// CAP.MPSMIN = 0 → 4 KiB page) so conformant hosts split larger I/O;
/// anything still over the cap is aborted with Invalid Field in Command.
const MAX_TRANSFER_BYTES: u32 = 1024 * 1024;

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

/// Wall-clock cap on the whole DH-HMAC-CHAP exchange. The controller
/// binding (CNTLID + association) is minted at Connect, *before* auth
/// runs; without a deadline a host that completes Connect and then
/// stalls mid-auth would pin that binding (and the connection task)
/// indefinitely — an unauthenticated slowloris that exhausts the
/// CNTLID space. 30 s is generous for a few small round-trips plus an
/// FFDHE keygen even on a slow host.
const AUTH_PHASE_TIMEOUT_SECS: u64 = 30;

/// FES codes (NVMe/TCP §3.6.4) we emit. Only the subset the server
/// can hit; full enumeration in the spec.
mod fes {
    /// Invalid PDU Header Field — e.g. PFV != 0 in ICReq.
    pub const INVALID_PDU_HEADER_FIELD: u16 = 0x01;
    /// PDU Sequence Error — wrong PDU type for this phase
    /// (e.g. CapsuleCmd before Connect succeeds).
    pub const PDU_SEQUENCE_ERROR: u16 = 0x02;
    /// Header Digest Error — inbound header digest didn't verify
    /// (negotiated but absent / mismatched). Fatal: the header can't be
    /// trusted, so the offending command can't be isolated.
    pub const HEADER_DIGEST_ERROR: u16 = 0x03;
    /// Invalid PDU Header Type — opcode byte we don't recognize.
    pub const INVALID_PDU_HEADER_TYPE: u16 = 0x07;
}

/// Max concurrent in-flight commands per connection. Comfortable
/// headroom under `CAP.MQES=1024`; a host that pipelines more gets a
/// `Namespace Not Ready` CQE instead of a spawned task. Enforced for
/// EVERY data-path command (not just R2T-needing writes) via a
/// per-connection semaphore so a flood of pipelined reads — each
/// buffering up to `MAX_TRANSFER_BYTES` of response data in a blocked
/// task — can't grow memory without bound (issue #178).
const INFLIGHT_CAP: usize = 256;

/// Max outstanding Async Event Requests a connection may hold parked.
/// Each parked AER costs a oneshot channel + a blocked delivery task; a
/// host streaming AER capsules would otherwise grow daemon memory
/// without bound (issue #177). Generous relative to any real host's
/// outstanding-AER count; the excess completes with SC 0x05 (Async
/// Event Request Limit Exceeded).
const AER_LIMIT: usize = 8;

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
    /// False when the PDU's data digest failed to verify (data digests
    /// negotiated). The per-command task drains the rest of the transfer
    /// off the wire, then completes the command with Data Transfer Error
    /// rather than dispatching corrupt data to the handler.
    digest_ok: bool,
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

#[allow(clippy::too_many_arguments)]
async fn serve_connection<S>(
    mut stream: S,
    peer: std::net::SocketAddr,
    local_addr: Option<std::net::SocketAddr>,
    handler: Arc<dyn NvmeCommandHandler>,
    controller_regs: Arc<ControllerRegs>,
    aer: Arc<ControllerRegistry>,
    tls_host_nqn: Option<String>,
    psks_path: Option<std::path::PathBuf>,
    dhchap_path: Option<std::path::PathBuf>,
    audit: Arc<dyn LoginAuditSink>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // State 1: Initialization — ICReq → ICResp.
    let icreq_pdu = pdu::RawPdu::read_async(&mut stream).await?;
    if icreq_pdu.header.pdu_type != pdu::PduType::ICReq {
        write_term_req(&mut stream, fes::PDU_SEQUENCE_ERROR, pdu::DigestCfg::NONE).await?;
        anyhow::bail!(
            "first PDU was {:?}, expected ICReq",
            icreq_pdu.header.pdu_type
        );
    }
    // Guard the slice: RawPdu::read_async only checks HLEN/PLEN framing,
    // so a host can send an 8-byte ICReq (PLEN=8) whose body is empty.
    // Slicing `[..PAYLOAD_LEN]` would then panic and tear down the
    // pre-auth connection task on crafted input (issue #176). Emit the
    // same INVALID_PDU_HEADER_FIELD term-req the length error would.
    if icreq_pdu.body.len() < pdu::ICReq::PAYLOAD_LEN {
        write_term_req(
            &mut stream,
            fes::INVALID_PDU_HEADER_FIELD,
            pdu::DigestCfg::NONE,
        )
        .await?;
        anyhow::bail!(
            "ICReq body too short: {} < {}",
            icreq_pdu.body.len(),
            pdu::ICReq::PAYLOAD_LEN
        );
    }
    let icreq = pdu::ICReq::read_from(&icreq_pdu.body[..pdu::ICReq::PAYLOAD_LEN])?;
    if icreq.pfv != 0 {
        write_term_req(
            &mut stream,
            fes::INVALID_PDU_HEADER_FIELD,
            pdu::DigestCfg::NONE,
        )
        .await?;
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
    // treat as 1 outstanding R2T per command". The server only ever
    // issues one R2T per command anyway, so this is purely a sanity floor.
    let host_maxr2t = icreq.maxr2t.max(1);
    // Digest negotiation (NVMe/TCP §3.4): the host requests header /
    // data digests in ICReq.dgst; we support both, so we honor whatever
    // it asked for and never require digests it didn't (a host that
    // sends dgst=0 keeps the no-digest fast path). The agreed config is
    // echoed in ICResp and threaded through every subsequent PDU.
    let dgst = pdu::DigestCfg::from_dgst_byte(icreq.dgst);
    let icresp = pdu::ICResp {
        pfv: 0,
        cpda: 0,
        dgst: dgst.to_dgst_byte(),
        maxh2cdata: ADVERTISED_MAXH2CDATA,
    };
    // ICResp itself carries no digest — digests apply to PDUs *after*
    // it, once both sides have agreed.
    stream.write_all(&icresp.to_pdu()).await?;
    stream.flush().await?;

    // State 2: Admission — Connect (first CapsuleCmd).
    let connect_pdu = pdu::RawPdu::read_async(&mut stream).await?;
    // First PDU after ICResp — digests (if negotiated) now apply. A bad
    // header digest is fatal; a bad data digest fails the Connect.
    if connect_pdu.verify_header_digest(dgst).is_err() {
        write_term_req(&mut stream, fes::HEADER_DIGEST_ERROR, dgst).await?;
        anyhow::bail!("Connect header digest verification failed");
    }
    let connect_data_ok = connect_pdu.verify_data_digest(dgst).is_ok();
    if connect_pdu.header.pdu_type != pdu::PduType::CapsuleCmd {
        write_term_req(&mut stream, fes::PDU_SEQUENCE_ERROR, dgst).await?;
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
        send_pdu(&mut stream, pdu::build_capsule_resp_pdu(&cqe), dgst).await?;
        anyhow::bail!("first command was not Admin Fabrics");
    }
    let fctype = nvme_base::fabrics::extract_fctype(&sqe);
    if fctype != Some(FabricsType::Connect) {
        let cqe = Cqe::failure(sqe.cid, 0, 0, StatusField::connect_invalid_parameters());
        send_pdu(&mut stream, pdu::build_capsule_resp_pdu(&cqe), dgst).await?;
        anyhow::bail!("first Fabrics command was not Connect ({:?})", fctype);
    }
    // A corrupted Connect Data payload (data digest mismatch) can't be
    // trusted to admit the host — refuse before parsing it.
    if !connect_data_ok {
        let cqe = Cqe::failure(sqe.cid, 0, 0, StatusField::connect_invalid_parameters());
        send_pdu(&mut stream, pdu::build_capsule_resp_pdu(&cqe), dgst).await?;
        anyhow::bail!("Connect data digest verification failed");
    }
    // QID lives at CDW10[31:16] — RECFMT at CDW10[15:0] is the
    // "Connection record format" version; we accept only 0.
    let recfmt = (sqe.cdw10 & 0xFFFF) as u16;
    if recfmt != 0 {
        let cqe = Cqe::failure(sqe.cid, 0, 0, StatusField::connect_invalid_parameters());
        send_pdu(&mut stream, pdu::build_capsule_resp_pdu(&cqe), dgst).await?;
        anyhow::bail!("unsupported Connect RECFMT={}", recfmt);
    }
    let qid = ((sqe.cdw10 >> 16) & 0xFFFF) as u16;
    let connect_data = match data_out {
        Some(d) if d.len() == ConnectData::WIRE_LEN => ConnectData::parse(d)?,
        _ => {
            let cqe = Cqe::failure(sqe.cid, 0, 0, StatusField::connect_invalid_parameters());
            send_pdu(&mut stream, pdu::build_capsule_resp_pdu(&cqe), dgst).await?;
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
        send_pdu(&mut stream, pdu::build_capsule_resp_pdu(&cqe), dgst).await?;
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
        send_pdu(&mut stream, pdu::build_capsule_resp_pdu(&cqe), dgst).await?;
        anyhow::bail!("HostNQN TLS/Connect mismatch");
    }
    // Bind this connection to a controller. An admin-queue Connect
    // (QID 0) creates a new controller and is assigned a fresh CNTLID;
    // an I/O-queue Connect (QID > 0) attaches to the controller the
    // host names in Connect Data CNTLID (the value it received from its
    // admin Connect). An unknown / mismatched CNTLID on an I/O Connect
    // is refused with Connect Invalid Parameters.
    let conn_token = if qid == 0 {
        aer.connect_admin(connect_data.hostid)
    } else {
        aer.connect_io(connect_data.hostid, connect_data.requested_cntlid)
    };
    let conn_token = match conn_token {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                peer = %peer,
                qid,
                requested_cntlid = connect_data.requested_cntlid,
                error = %e,
                "nvme-tcp: Connect controller binding refused",
            );
            let cqe = Cqe::failure(sqe.cid, 0, 0, StatusField::connect_invalid_parameters());
            send_pdu(&mut stream, pdu::build_capsule_resp_pdu(&cqe), dgst).await?;
            anyhow::bail!("Connect controller binding refused: {e}");
        }
    };
    let cntlid = conn_token.cntlid();
    // Assert ATR (Connect Response DW0 bit 17, Authentication Transaction
    // Required) when DH-HMAC-CHAP is configured: the host must complete the
    // Authentication Send/Receive exchange before any other command on this
    // queue.
    let auth_required = dhchap_path.is_some();
    let dw0 = nvme_base::fabrics::connect_response_dw0(cntlid, auth_required);
    let cqe = Cqe::success(sqe.cid, qid, 0, dw0);
    // The controller is now bound but we have not yet reached the State 3
    // teardown that calls `disconnect`. If writing the Connect Response
    // fails (host gone), release the binding before bailing — otherwise
    // the controller's CNTLID + association leak in the registry with no
    // teardown path to reclaim them.
    if let Err(e) = send_pdu(&mut stream, pdu::build_capsule_resp_pdu(&cqe), dgst).await {
        aer.disconnect(conn_token);
        return Err(e.into());
    }

    // Per-hostnqn admission set. Loaded fresh post-Connect so
    // operator edits to `nvmetcp-psks.json` take effect on the next
    // new connection without restart.
    //
    // Under VSA's mandatory-admission model:
    //   `psks_path = None`  → no admission lookup → see everything.
    //                         Only set when TLS-PSK is off (no
    //                         authentication = no admission, mirror
    //                         of iSCSI no-CHAP).
    //   `psks_path = Some`  → TLS-PSK is on, every connection MUST
    //                         be fenced. Only entry-with-volumes is
    //                         see-something. Entry-without-volumes,
    //                         entry-missing, or file read error all
    //                         reduce to `Some(empty)` = see nothing.
    // When DH-HMAC-CHAP is configured the auth phase below supplies the
    // admission set (and bypasses the TLS-PSK lookup); otherwise the
    // TLS-PSK `volumes` lookup applies. Both reduce a missing / failed
    // lookup to `Some(empty)` = see-nothing.
    let admission_volumes: Option<Vec<String>> = if let Some(dhpath) = &dhchap_path {
        // State 2b: DH-HMAC-CHAP in-band authentication. Sequential
        // request/response on the still-unsplit stream, before State 3.
        // Bounded by AUTH_PHASE_TIMEOUT_SECS so a host that stalls
        // mid-exchange can't pin the controller binding minted at
        // Connect (slowloris CNTLID exhaustion).
        let auth_result = tokio::time::timeout(
            std::time::Duration::from_secs(AUTH_PHASE_TIMEOUT_SECS),
            run_auth_phase(
                &mut stream,
                peer,
                qid,
                handler.subnqn(),
                &connect_data.hostnqn,
                dhpath,
                audit.as_ref(),
                dgst,
            ),
        )
        .await;
        match auth_result {
            Ok(Ok(volumes)) => {
                tracing::info!(
                    peer = %peer,
                    host_nqn = %connect_data.hostnqn,
                    admitted_volumes = volumes.len(),
                    "nvme-tcp: DH-HMAC-CHAP authentication succeeded",
                );
                audit.record(LoginAuditEvent::DhchapSuccess {
                    peer: &peer.to_string(),
                    host_nqn: &connect_data.hostnqn,
                    admitted_volumes: volumes.len(),
                });
                Some(volumes)
            }
            Ok(Err(e)) => {
                // Release the controller binding minted at Connect so its
                // CNTLID / association don't leak on a failed auth. The
                // granular failure audit row + brute-force alert were
                // already emitted at the refusal site inside
                // `run_auth_phase`; the remaining errors reaching here are
                // transport I/O faults (host EOF mid-exchange), not auth
                // refusals, so they get no `chap_failure` counter bump.
                aer.disconnect(conn_token);
                tracing::warn!(
                    peer = %peer,
                    host_nqn = %connect_data.hostnqn,
                    error = %e,
                    "nvme-tcp: DH-HMAC-CHAP authentication failed - closing",
                );
                return Err(e);
            }
            Err(_elapsed) => {
                // Timeout is owned by serve_connection (it wraps
                // run_auth_phase), so the failure row + alert are emitted
                // here rather than inside the auth routine.
                audit.record(LoginAuditEvent::DhchapFailure {
                    peer: &peer.to_string(),
                    host_nqn: &connect_data.hostnqn,
                    reason: "timeout",
                    error: format!(
                        "DH-HMAC-CHAP authentication timed out after {AUTH_PHASE_TIMEOUT_SECS}s"
                    ),
                });
                aer.disconnect(conn_token);
                tracing::warn!(
                    peer = %peer,
                    host_nqn = %connect_data.hostnqn,
                    timeout_secs = AUTH_PHASE_TIMEOUT_SECS,
                    "nvme-tcp: DH-HMAC-CHAP authentication timed out - closing",
                );
                anyhow::bail!("DH-HMAC-CHAP authentication timed out");
            }
        }
    } else {
        match &psks_path {
            None => None,
            Some(p) => match crate::identity::admission_for(p, &connect_data.hostnqn) {
                Ok(Some(v)) => Some(v),
                Ok(None) => {
                    tracing::warn!(
                        peer = %peer,
                        host_nqn = %connect_data.hostnqn,
                        "nvme-tcp: TLS-PSK on but no admission entry for host — fencing to empty",
                    );
                    Some(Vec::new())
                }
                Err(e) => {
                    tracing::warn!(
                        peer = %peer,
                        host_nqn = %connect_data.hostnqn,
                        error = %e,
                        "nvme-tcp: failed to load admission table — fencing to empty",
                    );
                    Some(Vec::new())
                }
            },
        }
    };

    tracing::info!(
        peer = %peer,
        host_nqn = %connect_data.hostnqn,
        qid,
        cntlid,
        admission_fenced = admission_volumes.is_some(),
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
    // `conn_token` (minted above by connect_admin / connect_io) names
    // the controller association this connection drives. Parked AERs on
    // it are released at teardown via `disconnect`, which also frees the
    // controller once its last association drops.
    let (read_half, write_half) = tokio::io::split(stream);
    let (outbound_tx, outbound_rx) = mpsc::channel::<OutboundPdu>(OUTBOUND_CAPACITY);
    let commands: CommandTable = Arc::new(Mutex::new(HashMap::new()));
    // Global per-connection in-flight gate covering every spawned
    // data-path command (issue #178), and outstanding-AER counter
    // bounding parked Async Event Requests (issue #177).
    let inflight = Arc::new(tokio::sync::Semaphore::new(INFLIGHT_CAP));
    let aer_outstanding = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut writer = tokio::spawn(writer_task(write_half, outbound_rx, peer, qid, dgst));
    let mut reader = tokio::spawn(reader_task(
        read_half,
        peer,
        local_addr,
        Arc::clone(&handler),
        Arc::clone(&controller_regs),
        Arc::clone(&aer),
        conn_token,
        Arc::clone(&commands),
        Arc::clone(&inflight),
        Arc::clone(&aer_outstanding),
        outbound_tx,
        qid,
        admission_volumes.map(Arc::new),
        connect_data.hostid,
        dgst,
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
    //
    // Deliberately NOT touched here: reservation state. NVMe
    // reservations are keyed by HOSTID and a host spans many
    // connections; tearing down one connection must not drop the
    // host's registration / reservation. The iSCSI side now matches
    // this: persistent registrations are keyed by the stable initiator
    // port (IQN + ISID) and survive nexus loss too (issue #57). Release
    // happens only on explicit Reservation Release / Register-unregister
    // / Preempt, or — when PTPL/APTPL is not set — a daemon restart.
    commands.lock().await.clear();
    // Surrender this connection's controller association: its parked
    // AERs are released (oneshot senders drop, unblocking the delivery
    // tasks) and, once the controller's last association drops, its
    // CNTLID + notification log + FID 0x82 masks are freed. The host's
    // reservation registration is HOSTID-keyed and survives (see #54).
    aer.disconnect(conn_token);
    Ok(())
}

/// PDU demuxer — owns the read half of the connection, the per-CCCID
/// routing table, and the `pending_fused` slot. Spawns one async task
/// per CapsuleCmd; H2CData PDUs are forwarded to the matching per-
/// command task's mpsc::Receiver. Returns on host close (EOF /
/// H2CTermReq) or fatal protocol violation.
#[allow(clippy::too_many_arguments)]
async fn reader_task<R>(
    mut read: R,
    peer: std::net::SocketAddr,
    local_addr: Option<std::net::SocketAddr>,
    handler: Arc<dyn NvmeCommandHandler>,
    controller_regs: Arc<ControllerRegs>,
    aer: Arc<ControllerRegistry>,
    conn_token: ConnToken,
    commands: CommandTable,
    inflight: Arc<tokio::sync::Semaphore>,
    aer_outstanding: Arc<std::sync::atomic::AtomicUsize>,
    outbound: mpsc::Sender<OutboundPdu>,
    qid: u16,
    admission_volumes: Option<Arc<Vec<String>>>,
    // 128-bit Host Identifier from Connect; names the reservation
    // registrant. Copy, so it threads into every per-command task.
    host_id: [u8; 16],
    dgst: pdu::DigestCfg,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    // CNTLID of the controller this connection drives — threaded into
    // every admin command so the per-controller AER state (Identify,
    // FID 0x82 mask, LID 0x80 log) is keyed correctly.
    let cntlid = conn_token.cntlid();
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
        // Header digest (if negotiated) gates everything else: a bad
        // header means we can't trust the PDU type / CCCID, so it's a
        // fatal connection error (NVMe/TCP §3.4). Data digests are
        // checked per-arm below, where the owning command is known.
        if let Err(e) = raw.verify_header_digest(dgst) {
            tracing::warn!(peer = %peer, error = %e, "nvme-tcp: header digest error");
            let _ = outbound
                .send(OutboundPdu::TermReq {
                    fes: fes::HEADER_DIGEST_ERROR,
                })
                .await;
            return Err(e.into());
        }
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
                // Data digest (if negotiated) verified here, before the
                // chunk reaches the per-command task. A mismatch isn't
                // fatal — the chunk is forwarded flagged so the owning
                // command completes with Data Transfer Error while the
                // connection stays up (NVMe/TCP §3.4).
                let digest_ok = match raw.verify_data_digest(dgst) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(
                            peer = %peer,
                            cccid = h2c.cccid,
                            error = %e,
                            "nvme-tcp: H2CData data digest error - failing command",
                        );
                        false
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
                            digest_ok,
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

                // In-capsule data digest (if negotiated + present): a
                // mismatch fails just this command with Data Transfer
                // Error; the connection survives (NVMe/TCP §3.4).
                if let Err(e) = raw.verify_data_digest(dgst) {
                    tracing::warn!(
                        peer = %peer,
                        cid = sqe.cid,
                        error = %e,
                        "nvme-tcp: CapsuleCmd data digest error - failing command",
                    );
                    let cqe = Cqe::failure(sqe.cid, 0, 0, StatusField::data_transfer_error());
                    let _ = outbound
                        .send(OutboundPdu::CommandResponse {
                            cqe,
                            data_in: Vec::new(),
                        })
                        .await;
                    continue;
                }

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

                // Async Event Request (admin queue only). AER never
                // completes synchronously — it parks until a controller
                // event fires — so it bypasses `handle_command` (which
                // always emits exactly one CommandResponse). A delivery
                // task awaits the ControllerRegistry oneshot and, on
                // completion, emits the CQE on this connection's writer.
                // Its DW0 points the host at the reservation notification
                // log (LID 0x80). On the Err path (sender dropped at
                // teardown via `disconnect`) it emits nothing — AER may
                // legally never complete.
                if qid == 0
                    && AdminOpcode::from_u8(sqe.opcode) == Some(AdminOpcode::AsyncEventRequest)
                {
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
                    use std::sync::atomic::Ordering;
                    // Bound outstanding AERs (issue #177): excess completes
                    // with SC 0x05 rather than parking a oneshot + blocking
                    // a task per capsule without limit.
                    if aer_outstanding.load(Ordering::Acquire) >= AER_LIMIT {
                        let _ = outbound
                            .send(OutboundPdu::CommandResponse {
                                cqe: Cqe::failure(
                                    sqe.cid,
                                    0,
                                    0,
                                    StatusField::async_event_limit_exceeded(),
                                ),
                                data_in: Vec::new(),
                            })
                            .await;
                        continue;
                    }
                    aer_outstanding.fetch_add(1, Ordering::AcqRel);
                    let (tx, rx) = oneshot::channel();
                    aer.park(conn_token, tx);
                    let outbound_clone = outbound.clone();
                    let cid = sqe.cid;
                    let aer_outstanding_clone = Arc::clone(&aer_outstanding);
                    tokio::spawn(async move {
                        if let Ok(completion) = rx.await {
                            let _ = outbound_clone
                                .send(OutboundPdu::CommandResponse {
                                    cqe: Cqe::success(cid, 0, 0, completion.dw0),
                                    data_in: Vec::new(),
                                })
                                .await;
                        }
                        // Slot freed whether the AER delivered or the
                        // connection tore down (sender dropped).
                        aer_outstanding_clone.fetch_sub(1, Ordering::AcqRel);
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

                // Bound the host-declared transfer before it reaches the
                // `vec![0u8; sgl_len]` in handle_command. Exceeding the
                // advertised MDTS is aborted with Invalid Field in
                // Command (NVMe Base §5.17.2.1) rather than allocating.
                if sgl_len > MAX_TRANSFER_BYTES {
                    tracing::warn!(
                        peer = %peer,
                        sgl_len,
                        cap = MAX_TRANSFER_BYTES,
                        "nvme-tcp: command transfer length exceeds MDTS cap",
                    );
                    let cqe = Cqe::failure(sqe.cid, 0, 0, StatusField::invalid_field());
                    let _ = outbound
                        .send(OutboundPdu::CommandResponse {
                            cqe,
                            data_in: Vec::new(),
                        })
                        .await;
                    continue;
                }

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
                    let admission_clone = admission_volumes.clone();
                    tokio::spawn(async move {
                        let admission_slice = admission_clone.as_deref().map(|v| v.as_slice());
                        let (cqe_c, cqe_w) = handler_clone
                            .handle_fused_compare_write(
                                IoCommand {
                                    sqe: compare_sqe,
                                    data_out: Some(&compare_data),
                                    // Cap at MDTS so the dispatcher's
                                    // transfer-length check rejects an
                                    // oversized NLB instead of allocating
                                    // up to 256 MiB (issue #127).
                                    data_in_max: MAX_TRANSFER_BYTES,
                                    session_volumes: admission_slice,
                                    host_id: Some(host_id),
                                },
                                IoCommand {
                                    sqe,
                                    data_out: Some(&write_data),
                                    data_in_max: MAX_TRANSFER_BYTES,
                                    session_volumes: admission_slice,
                                    host_id: Some(host_id),
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

                // Global in-flight gate (issue #178): EVERY spawned
                // data-path command holds a permit for its lifetime, not
                // just R2T-needing writes — otherwise a flood of
                // pipelined reads (each buffering up to MAX_TRANSFER_BYTES
                // of response data in a blocked task) grows memory without
                // bound. Over cap → Namespace Not Ready, no task spawned.
                let permit = match Arc::clone(&inflight).try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        let cqe =
                            Cqe::failure(sqe.cid, 0, 0, StatusField::namespace_not_ready());
                        let _ = outbound
                            .send(OutboundPdu::CommandResponse {
                                cqe,
                                data_in: Vec::new(),
                            })
                            .await;
                        continue;
                    }
                };

                // Decide R2T need; R2T-needing commands also consume a
                // slot in the per-connection h2c routing table.
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
                let admission_clone = admission_volumes.clone();
                tokio::spawn(async move {
                    // Hold the in-flight permit for the command's whole
                    // lifetime; it releases when this task ends (#178).
                    let _permit = permit;
                    handle_command(
                        sqe,
                        icd_owned,
                        sgl_len,
                        qid,
                        h2c_rx,
                        handler_clone,
                        outbound_clone,
                        commands_clone,
                        peer,
                        local_addr,
                        admission_clone,
                        host_id,
                        cntlid,
                    )
                    .await
                });
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
    qid: u16,
    dgst: pdu::DigestCfg,
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
                    .write_all(&pdu::apply_digests(
                        pdu::build_r2t_pdu(cccid, ttag, r2to, r2tl),
                        dgst,
                    ))
                    .await?;
                write.flush().await?;
            }
            OutboundPdu::CommandResponse { mut cqe, data_in } => {
                // Stamp the connection's QID into SQID so steady-state
                // completions match the Connect Response + auth phases
                // (issue #72). The command-set layers (nvme-nvm, fabrics)
                // build CQEs with SQID=0 because queue ids are a transport
                // concern they don't know about; the transport owns the
                // mapping and overrides it here — the one chokepoint every
                // steady-state completion (data path, AER, Property /
                // Identify / probe, fused-error) funnels through. On the
                // admin queue QID is 0, so this is a no-op there.
                cqe.sqid = qid;
                if !data_in.is_empty() {
                    // SUCCESS-bit optimization (NVMe/TCP §3.6.7): if
                    // the CQE is a no-payload success, fold completion
                    // into the C2HData and skip the trailing
                    // CapsuleResp. One round-trip saved on every
                    // Identify / Get Log Page / NVM Read.
                    let can_fold =
                        cqe.status == StatusField::SUCCESS && cqe.dw0 == 0 && cqe.dw1 == 0;
                    let extra = if can_fold { pdu::C2H_FLAGS_SUCCESS } else { 0 };
                    // Zero-copy emit: write the small header, then the
                    // borrowed payload, then the optional data digest —
                    // the (up to 128 KiB) read payload is never copied
                    // into a PDU buffer (issue #242).
                    let header =
                        pdu::build_c2hdata_header(cqe.cid, data_in.len(), extra, dgst);
                    write.write_all(&header).await?;
                    write.write_all(&data_in).await?;
                    if dgst.data && !data_in.is_empty() {
                        let crc = crc32c::crc32c(&data_in).to_le_bytes();
                        write.write_all(&crc).await?;
                    }
                    if !can_fold {
                        write
                            .write_all(&pdu::apply_digests(pdu::build_capsule_resp_pdu(&cqe), dgst))
                            .await?;
                    }
                } else {
                    write
                        .write_all(&pdu::apply_digests(pdu::build_capsule_resp_pdu(&cqe), dgst))
                        .await?;
                }
                write.flush().await?;
            }
            OutboundPdu::TermReq { fes } => {
                let _ = write
                    .write_all(&pdu::apply_digests(pdu::build_c2h_term_req_pdu(fes), dgst))
                    .await;
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
#[allow(clippy::too_many_arguments)]
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
    local_addr: Option<std::net::SocketAddr>,
    admission_volumes: Option<Arc<Vec<String>>>,
    host_id: [u8; 16],
    cntlid: u16,
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
        // Set if any H2CData chunk failed its data digest. We keep
        // draining the transfer off the wire (so trailing H2CData PDUs
        // don't strand as unknown-CCCID protocol errors), then complete
        // the command with Data Transfer Error instead of dispatching.
        let mut data_corrupt = false;
        while received < r2tl {
            let chunk = match rx.recv().await {
                Some(c) => c,
                None => {
                    // Reader is gone (connection tearing down) — drop.
                    remove_from_table(&commands, cccid).await;
                    return;
                }
            };
            data_corrupt |= !chunk.digest_ok;
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
        if data_corrupt {
            // Transfer arrived but at least one PDU's data digest didn't
            // verify — fail the command (host may retry; DNR clear).
            let _ = outbound
                .send(OutboundPdu::CommandResponse {
                    cqe: Cqe::failure(cccid, qid, 0, StatusField::data_transfer_error()),
                    data_in: Vec::new(),
                })
                .await;
            return;
        }
        Some(assembled)
    } else if matches!(data_direction(sqe.opcode), DataDirection::HostToController) {
        // Fully in-capsule (ICD covers the SGL length, or SGL = 0).
        Some(icd)
    } else {
        None
    };

    let data_out = buf.as_deref();
    let admission_slice = admission_volumes.as_deref().map(|v| v.as_slice());
    let response = if qid == 0 {
        handler
            .handle_admin(AdminCommand {
                sqe,
                data_out,
                data_in_max: u32::MAX,
                session_volumes: admission_slice,
                cntlid: Some(cntlid),
                local_addr,
            })
            .await
    } else {
        handler
            .handle_io(IoCommand {
                sqe,
                data_out,
                // Cap at MDTS so the dispatcher rejects an oversized NLB
                // read (NLB drives the read length, not the SGL the MDTS
                // reader-check inspects) instead of allocating up to
                // 256 MiB from a single small capsule (issue #127).
                data_in_max: MAX_TRANSFER_BYTES,
                session_volumes: admission_slice,
                host_id: Some(host_id),
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

// ===================== DH-HMAC-CHAP auth phase =====================
//
// Runs on the still-unsplit stream between the Connect Response and the
// State-3 reader/writer split, only when DH-HMAC-CHAP is configured.
// The exchange is strictly serialized — Negotiate/Challenge/Reply/
// Success1/Success2 — so the simple read/write style of the Connect
// phase suffices. The wire (de)serialization is `nvme_base::auth`
// (`dhwire`); the HMAC / Diffie-Hellman is `crate::auth` (`dhcrypt`).

/// Negotiated DH-HMAC-CHAP parameters carried from Challenge to the
/// Reply validation.
#[derive(Clone, Copy)]
struct NegotiatedParams {
    hash_id: u8,
    hash_len: usize,
    dhgid: u8,
}

/// Pick the strongest hash the host offered (SHA-512 > 384 > 256).
fn select_hash(offered: &[u8]) -> Option<u8> {
    [
        dhwire::NVME_AUTH_HASH_SHA512,
        dhwire::NVME_AUTH_HASH_SHA384,
        dhwire::NVME_AUTH_HASH_SHA256,
    ]
    .into_iter()
    .find(|h| offered.contains(h))
}

/// Pick the DH group, preferring real Diffie-Hellman at a sane cost.
///
/// We prefer ffdhe3072 (NIST ~128-bit security, a few-ms keygen), then
/// 4096, then 2048, then the heavy 6144/8192 only if the host offers
/// nothing smaller, then NULL last. Deliberately *not* "strongest
/// first": the controller mints an ephemeral keypair for the chosen
/// group before the host proves any secret, and a per-connection
/// 8192-bit keygen is a needless CPU cost on every legitimate connect
/// (the Linux host offers every group, so we'd otherwise always land on
/// 8192). The auth-phase timeout bounds the abuse window for a host
/// that offers only a heavy group.
fn select_dhgroup(offered: &[u8]) -> Option<u8> {
    [
        dhwire::NVME_AUTH_DHGROUP_3072,
        dhwire::NVME_AUTH_DHGROUP_4096,
        dhwire::NVME_AUTH_DHGROUP_2048,
        dhwire::NVME_AUTH_DHGROUP_6144,
        dhwire::NVME_AUTH_DHGROUP_8192,
        dhwire::NVME_AUTH_DHGROUP_NULL,
    ]
    .into_iter()
    .find(|g| offered.contains(g))
}

/// Read one CapsuleCmd that must be an Authentication Send / Receive
/// Fabrics command of the expected FCTYPE; returns the SQE plus the
/// in-capsule message bytes (empty for Authentication Receive). A
/// non-conforming PDU is a transport violation -> C2HTermReq + bail.
async fn recv_auth_capsule<S>(
    stream: &mut S,
    expect: FabricsType,
    dgst: pdu::DigestCfg,
) -> Result<(Sqe, Vec<u8>)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let raw = pdu::RawPdu::read_async(stream).await?;
    // Digests (if negotiated) apply to auth PDUs too — a bad header is
    // fatal; a corrupt auth payload (data digest) can't be trusted to
    // drive the exchange, so bail and let the connection close.
    if raw.verify_header_digest(dgst).is_err() {
        write_term_req(stream, fes::HEADER_DIGEST_ERROR, dgst).await?;
        anyhow::bail!("auth: header digest verification failed");
    }
    if raw.verify_data_digest(dgst).is_err() {
        anyhow::bail!("auth: data digest verification failed");
    }
    if raw.header.pdu_type != pdu::PduType::CapsuleCmd {
        write_term_req(stream, fes::PDU_SEQUENCE_ERROR, dgst).await?;
        anyhow::bail!("auth: expected CapsuleCmd, got {:?}", raw.header.pdu_type);
    }
    let (sqe, data) = pdu::parse_capsule_cmd(&raw)?;
    if AdminOpcode::from_u8(sqe.opcode) != Some(AdminOpcode::Fabrics) {
        write_term_req(stream, fes::INVALID_PDU_HEADER_FIELD, dgst).await?;
        anyhow::bail!(
            "auth: expected Admin Fabrics opcode, got 0x{:02X}",
            sqe.opcode
        );
    }
    if nvme_base::fabrics::extract_fctype(&sqe) != Some(expect) {
        write_term_req(stream, fes::INVALID_PDU_HEADER_FIELD, dgst).await?;
        anyhow::bail!("auth: unexpected Fabrics command (wanted {:?})", expect);
    }
    Ok((sqe, data.map(|d| d.to_vec()).unwrap_or_default()))
}

/// ACK an Authentication Send command with a success CapsuleResp.
/// In-band auth failures travel as an AUTH_Failure message on the next
/// Authentication Receive, not as a command error, so a well-formed
/// Send always completes successfully. `qid` is echoed in the CQE SQID
/// to match the Connect Response (consistent on I/O-queue auth).
async fn ack_auth_send<S>(stream: &mut S, cid: u16, qid: u16, dgst: pdu::DigestCfg) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    send_pdu(
        stream,
        pdu::build_capsule_resp_pdu(&Cqe::success(cid, qid, 0, 0)),
        dgst,
    )
    .await?;
    Ok(())
}

/// Send a controller->host auth message as the data-in of an
/// Authentication Receive command: a C2HData PDU carrying the message,
/// then the command's success CapsuleResp (SQID echoes `qid`).
///
/// `al` is the host-advertised Allocation Length from the Authentication
/// Receive's CDW11. If the controller message exceeds it, we cannot
/// honor the transfer, so the command is failed with Invalid Field in
/// Command rather than over-sending — and the auth phase bails. Our
/// messages stay well under any conformant host's AL (the Challenge is
/// <= ~1.1 KiB at FFDHE-8192), so this never trips in practice.
async fn send_auth_message<S>(
    stream: &mut S,
    cid: u16,
    qid: u16,
    al: u32,
    msg: &[u8],
    dgst: pdu::DigestCfg,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    if msg.len() as u64 > u64::from(al) {
        send_pdu(
            stream,
            pdu::build_capsule_resp_pdu(&Cqe::failure(cid, qid, 0, StatusField::invalid_field())),
            dgst,
        )
        .await?;
        anyhow::bail!(
            "auth: controller message {} bytes exceeds host allocation length {} bytes",
            msg.len(),
            al
        );
    }
    stream
        .write_all(&pdu::apply_digests(pdu::build_c2hdata_pdu(cid, msg), dgst))
        .await?;
    send_pdu(
        stream,
        pdu::build_capsule_resp_pdu(&Cqe::success(cid, qid, 0, 0)),
        dgst,
    )
    .await?;
    Ok(())
}

/// Validate the host's Reply (response R1) and, for mutual auth, build
/// the controller response R2 into a Success1 message. Returns the
/// Success1 message bytes on success, or an AUTH_Failure `rescode_exp`.
#[allow(clippy::too_many_arguments)]
fn validate_reply(
    reply_data: &[u8],
    params: &NegotiatedParams,
    t_id: u16,
    sc_c: u8,
    c1: &[u8],
    s1: u32,
    dh_keypair: Option<&dhcrypt::DhKeypair>,
    resolved: &crate::identity::ResolvedDhchap,
    subnqn: &str,
    hostnqn: &str,
) -> Result<Vec<u8>, u8> {
    let reply = dhwire::parse_reply(reply_data, params.hash_len)
        .map_err(|_| dhwire::NVME_AUTH_DHCHAP_FAILURE_INCORRECT_PAYLOAD)?;
    // DH session key (once), if a non-NULL group was negotiated.
    let session_key = match dh_keypair {
        Some(kp) => Some(
            kp.session_key(&reply.host_dh_value, params.hash_id)
                .map_err(|_| dhwire::NVME_AUTH_DHCHAP_FAILURE_FAILED)?,
        ),
        None => None,
    };
    let augment = |chal: &[u8]| -> Result<Vec<u8>, u8> {
        match &session_key {
            Some(sk) => dhcrypt::augmented_challenge(params.hash_id, sk, chal)
                .map_err(|_| dhwire::NVME_AUTH_DHCHAP_FAILURE_FAILED),
            None => Ok(chal.to_vec()),
        }
    };
    // Validate R1 against the current secret (and the previous one
    // during a rotation grace window).
    let c1_aug = augment(c1)?;
    let mut authenticated = false;
    for secret in &resolved.secrets {
        let tk = dhcrypt::transform_key(secret, hostnqn)
            .map_err(|_| dhwire::NVME_AUTH_DHCHAP_FAILURE_FAILED)?;
        let expected = dhcrypt::dhchap_response(&dhcrypt::ResponseInput {
            transformed_key: &tk,
            hash_id: params.hash_id,
            challenge: &c1_aug,
            seqnum: s1,
            t_id,
            sc_c,
            label: dhcrypt::LABEL_HOST,
            nqn_first: hostnqn,
            nqn_second: subnqn,
        })
        .map_err(|_| dhwire::NVME_AUTH_DHCHAP_FAILURE_FAILED)?;
        if dhcrypt::responses_equal(&expected, &reply.response) {
            authenticated = true;
            break;
        }
    }
    if !authenticated {
        return Err(dhwire::NVME_AUTH_DHCHAP_FAILURE_FAILED);
    }
    // Mutual auth: the host included a challenge C2 and expects the
    // controller to prove itself with R2 (needs a configured ctrl key).
    if let Some(c2) = &reply.host_challenge {
        let ctrl = resolved
            .ctrl_secret
            .as_ref()
            .ok_or(dhwire::NVME_AUTH_DHCHAP_FAILURE_FAILED)?;
        let c2_aug = augment(c2)?;
        let ctk = dhcrypt::transform_key(ctrl, subnqn)
            .map_err(|_| dhwire::NVME_AUTH_DHCHAP_FAILURE_FAILED)?;
        let r2 = dhcrypt::dhchap_response(&dhcrypt::ResponseInput {
            transformed_key: &ctk,
            hash_id: params.hash_id,
            challenge: &c2_aug,
            seqnum: reply.seqnum,
            t_id,
            sc_c,
            label: dhcrypt::LABEL_CONTROLLER,
            nqn_first: subnqn,
            nqn_second: hostnqn,
        })
        .map_err(|_| dhwire::NVME_AUTH_DHCHAP_FAILURE_FAILED)?;
        Ok(dhwire::build_success1(t_id, params.hash_len, Some(&r2)))
    } else {
        Ok(dhwire::build_success1(t_id, params.hash_len, None))
    }
}

/// Drive the controller side of the DH-HMAC-CHAP exchange. Returns the
/// host's admission `volumes` on success; any failure has already
/// emitted an AUTH_Failure (where the protocol allows) before the
/// returned error closes the connection.
#[allow(clippy::too_many_arguments)]
async fn run_auth_phase<S>(
    stream: &mut S,
    peer: std::net::SocketAddr,
    qid: u16,
    subnqn: &str,
    hostnqn: &str,
    dhchap_path: &std::path::Path,
    audit: &dyn LoginAuditSink,
    dgst: pdu::DigestCfg,
) -> Result<Vec<String>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Record a refused-auth audit row (and, via the daemon sink, bump
    // the brute-force alert counter) at each genuine auth-refusal site
    // below. Transport I/O faults (a host that drops mid-exchange) take
    // the `?` early-return paths and are deliberately *not* audited —
    // they are not credential refusals.
    let peer_str = peer.to_string();
    let emit_failure = |reason: &str, error: String| {
        audit.record(LoginAuditEvent::DhchapFailure {
            peer: &peer_str,
            host_nqn: hostnqn,
            reason,
            error,
        });
    };

    use dhwire::{
        NVME_AUTH_DHCHAP_FAILURE_CONCAT_MISMATCH as FAIL_CONCAT,
        NVME_AUTH_DHCHAP_FAILURE_DHGROUP_UNUSABLE as FAIL_DHGROUP,
        NVME_AUTH_DHCHAP_FAILURE_FAILED as FAIL_FAILED,
        NVME_AUTH_DHCHAP_FAILURE_HASH_UNUSABLE as FAIL_HASH,
        NVME_AUTH_DHCHAP_FAILURE_INCORRECT_PAYLOAD as FAIL_PAYLOAD,
        NVME_AUTH_DHCHAP_FAILURE_REASON_FAILED as REASON_FAILED,
    };

    // --- Negotiate (Authentication Send) ---
    let (neg_sqe, neg_data) =
        recv_auth_capsule(stream, FabricsType::AuthenticationSend, dgst).await?;
    let fields = dhwire::parse_auth_command(&neg_sqe);
    if fields.secp != dhwire::NVME_AUTH_DHCHAP_PROTOCOL_IDENTIFIER {
        tracing::debug!(
            peer = %peer,
            secp = fields.secp,
            "nvme-tcp: auth: unexpected SECP (continuing)",
        );
    }
    // Well-formed Send always ACKs; failures surface on the next Receive.
    let neg = dhwire::parse_negotiate(&neg_data);
    ack_auth_send(stream, neg_sqe.cid, qid, dgst).await?;

    // Compute the negotiation outcome: parameters + resolved secret, or
    // a failure explanation code to return in AUTH_Failure.
    let (t_id, sc_c, outcome): (
        u16,
        u8,
        Result<(NegotiatedParams, crate::identity::ResolvedDhchap), u8>,
    ) = match neg {
        // Echo the host's t_id even on a parse failure (it sits at a
        // fixed offset, recoverable when the buffer reached it) so a
        // host that demultiplexes auth responses by transaction id can
        // match the Failure to its request.
        Err(_) => (dhwire::peek_t_id(&neg_data), 0, Err(FAIL_PAYLOAD)),
        Ok(n) => {
            let t_id = n.t_id;
            let sc_c = n.sc_c;
            let result = (|| {
                if sc_c != 0 {
                    // We do not implement secure-channel concatenation.
                    return Err(FAIL_CONCAT);
                }
                let desc = n.dhchap_descriptor().ok_or(FAIL_PAYLOAD)?;
                let hash_id = select_hash(&desc.hash_ids).ok_or(FAIL_HASH)?;
                let hash_len = dhwire::hash_len(hash_id).ok_or(FAIL_HASH)?;
                let dhgid = select_dhgroup(&desc.dhgroup_ids).ok_or(FAIL_DHGROUP)?;
                // A read/parse/bad-key error in the secret store is an
                // operator misconfiguration, not "unknown host" — log it
                // distinctly (the host still gets a generic AUTH_Failure;
                // we never leak which case it was on the wire).
                let resolved = match crate::identity::dhchap_lookup(dhchap_path, hostnqn) {
                    Ok(Some(r)) => r,
                    Ok(None) => return Err(FAIL_FAILED),
                    Err(e) => {
                        tracing::warn!(
                            peer = %peer,
                            host_nqn = %hostnqn,
                            error = %e,
                            "nvme-tcp: DH-HMAC-CHAP secret store unreadable or malformed - refusing",
                        );
                        return Err(FAIL_FAILED);
                    }
                };
                Ok((
                    NegotiatedParams {
                        hash_id,
                        hash_len,
                        dhgid,
                    },
                    resolved,
                ))
            })();
            (t_id, sc_c, result)
        }
    };

    // --- Challenge (Authentication Receive pulls it) ---
    let (recv1_sqe, _) =
        recv_auth_capsule(stream, FabricsType::AuthenticationReceive, dgst).await?;
    let recv1_al = dhwire::parse_auth_command(&recv1_sqe).tl_al;
    let (params, resolved) = match outcome {
        Err(reason) => {
            // Record the refusal (audit row + brute-force counter) before
            // attempting delivery: this is a genuine credential refusal, so
            // the security signal must not hinge on the Failure1 send
            // succeeding — e.g. a host that advertised a too-small AL makes
            // `send_auth_message` bail before any post-send code runs.
            emit_failure(
                "negotiation_failed",
                format!("DH-HMAC-CHAP negotiation failed (reason_exp 0x{reason:02X})"),
            );
            send_auth_message(
                stream,
                recv1_sqe.cid,
                qid,
                recv1_al,
                &dhwire::build_failure1(t_id, REASON_FAILED, reason),
                dgst,
            )
            .await?;
            anyhow::bail!("DH-HMAC-CHAP negotiation failed (reason_exp 0x{reason:02X})");
        }
        Ok(v) => v,
    };

    let c1 =
        dhcrypt::random_bytes(params.hash_len).map_err(|e| anyhow::anyhow!("auth rng: {e}"))?;
    let s1 = dhcrypt::random_seqnum().map_err(|e| anyhow::anyhow!("auth rng: {e}"))?;
    // FFDHE keygen is a modular exponentiation that can take low single-
    // digit milliseconds (more for a heavy group) — run it off the
    // reactor so a connection flood negotiating a large group can't stall
    // other tasks on this worker thread.
    let dh_keypair = if params.dhgid != dhwire::NVME_AUTH_DHGROUP_NULL {
        let dhgid = params.dhgid;
        let kp = tokio::task::spawn_blocking(move || dhcrypt::DhKeypair::generate(dhgid))
            .await
            .map_err(|e| anyhow::anyhow!("auth dh keygen task: {e}"))?
            .map_err(|e| anyhow::anyhow!("auth dh: {e}"))?;
        Some(kp)
    } else {
        None
    };
    let dhval = match &dh_keypair {
        Some(kp) => kp
            .public_value()
            .map_err(|e| anyhow::anyhow!("auth dh: {e}"))?,
        None => Vec::new(),
    };
    let challenge = dhwire::build_challenge(t_id, params.hash_id, params.dhgid, s1, &c1, &dhval);
    send_auth_message(stream, recv1_sqe.cid, qid, recv1_al, &challenge, dgst).await?;

    // --- Reply (Authentication Send) ---
    let (reply_sqe, reply_data) =
        recv_auth_capsule(stream, FabricsType::AuthenticationSend, dgst).await?;
    ack_auth_send(stream, reply_sqe.cid, qid, dgst).await?;
    // Validate R1 (and build R2 for mutual auth) off the reactor: the
    // DH session-key derivation is a full-width modular exponentiation,
    // and the HMACs ride along. `validate_reply` is pure CPU, so move
    // owned copies of its inputs into the blocking task. `resolved` is
    // cloned (cheap — a few key blobs) so the original survives for the
    // `volumes` return below; the ephemeral keypair is consumed here.
    let result2 = {
        let c1 = c1.clone();
        let resolved = resolved.clone();
        let subnqn = subnqn.to_string();
        let hostnqn = hostnqn.to_string();
        tokio::task::spawn_blocking(move || {
            validate_reply(
                &reply_data,
                &params,
                t_id,
                sc_c,
                &c1,
                s1,
                dh_keypair.as_ref(),
                &resolved,
                &subnqn,
                &hostnqn,
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!("auth reply-validation task: {e}"))?
    };

    // --- Success1 / Failure (Authentication Receive pulls it) ---
    let (recv2_sqe, _) =
        recv_auth_capsule(stream, FabricsType::AuthenticationReceive, dgst).await?;
    let recv2_al = dhwire::parse_auth_command(&recv2_sqe).tl_al;
    match result2 {
        Err(reason) => {
            // Emit before delivery (see the negotiation-failure site): a
            // failed R1 is a genuine refusal whose audit row + brute-force
            // counter must fire regardless of whether the Failure1 send
            // succeeds (e.g. a too-small host AL bails the send).
            emit_failure(
                "reply_invalid",
                format!("DH-HMAC-CHAP host response invalid (reason_exp 0x{reason:02X})"),
            );
            send_auth_message(
                stream,
                recv2_sqe.cid,
                qid,
                recv2_al,
                &dhwire::build_failure1(t_id, REASON_FAILED, reason),
                dgst,
            )
            .await?;
            anyhow::bail!("DH-HMAC-CHAP host response invalid (reason_exp 0x{reason:02X})");
        }
        Ok(success1) => {
            send_auth_message(stream, recv2_sqe.cid, qid, recv2_al, &success1, dgst).await?
        }
    }

    // --- Success2 (or Failure2 if the host rejected our R2) ---
    let (final_sqe, final_data) =
        recv_auth_capsule(stream, FabricsType::AuthenticationSend, dgst).await?;
    ack_auth_send(stream, final_sqe.cid, qid, dgst).await?;
    match dhwire::peek_message_type(&final_data) {
        Some((dhwire::NVME_AUTH_COMMON_MESSAGES, dhwire::NVME_AUTH_DHCHAP_MESSAGE_FAILURE1))
        | Some((dhwire::NVME_AUTH_COMMON_MESSAGES, dhwire::NVME_AUTH_DHCHAP_MESSAGE_FAILURE2)) => {
            emit_failure(
                "controller_rejected",
                "host rejected controller authentication".to_string(),
            );
            anyhow::bail!("host rejected controller authentication");
        }
        _ => {
            // Success2 carries no HMAC, so its only transaction binding
            // is the t_id field — require it to match the negotiated
            // transaction (the rest of the exchange is bound via the
            // response HMACs).
            let s2_tid = dhwire::parse_success2(&final_data)?;
            if s2_tid != t_id {
                emit_failure(
                    "success2_tid_mismatch",
                    format!("DH-HMAC-CHAP Success2 t_id {s2_tid:#06x} != negotiated {t_id:#06x}"),
                );
                anyhow::bail!("DH-HMAC-CHAP Success2 t_id {s2_tid:#06x} != negotiated {t_id:#06x}");
            }
        }
    }
    Ok(resolved.volumes)
}

/// Best-effort C2HTermReq write before closing. Used by the State 1
/// / State 2 admission code (ICReq / Connect) — once State 3 has
/// split the stream, the writer task owns all PDU emission and
/// fatal errors travel as `OutboundPdu::TermReq` through the channel.
/// `dgst` is `NONE` for the pre-ICResp State-1 callers (no negotiation
/// yet) and the negotiated config for everything after.
async fn write_term_req<S>(stream: &mut S, fes: u16, dgst: pdu::DigestCfg) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let _ = send_pdu(stream, pdu::build_c2h_term_req_pdu(fes), dgst).await;
    Ok(())
}

/// Write one built PDU to the (unsplit) stream during the handshake /
/// auth phase, applying the negotiated digests and flushing. The
/// steady-state path uses the writer task instead; this is only for the
/// sequential pre-State-3 PDUs.
async fn send_pdu<S>(stream: &mut S, pdu: Vec<u8>, dgst: pdu::DigestCfg) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    stream.write_all(&pdu::apply_digests(pdu, dgst)).await?;
    stream.flush().await
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
    /// can be exercised without pulling in core-block + shared-object-store.
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
        let aer = Arc::new(ControllerRegistry::new());
        tokio::spawn(async move {
            let _ = accept_loop(
                listener,
                h,
                regs,
                aer,
                None,
                None,
                None,
                Arc::new(NoopLoginAudit),
            )
            .await;
        });
        port
    }

    /// Test sink that captures every login-audit event so a test can
    /// assert the transport recorded an auth refusal (the daemon-side
    /// `chap_failure` alert + audit row are driven off the same hook).
    #[derive(Default)]
    struct CapturingAudit {
        // (kind, reason_or_empty, host_nqn)
        events: std::sync::Mutex<Vec<(String, String, String)>>,
    }
    impl LoginAuditSink for CapturingAudit {
        fn record(&self, event: LoginAuditEvent<'_>) {
            let row = match event {
                LoginAuditEvent::DhchapSuccess { host_nqn, .. } => {
                    ("success".to_string(), String::new(), host_nqn.to_string())
                }
                LoginAuditEvent::DhchapFailure {
                    host_nqn, reason, ..
                } => (
                    "failure".to_string(),
                    reason.to_string(),
                    host_nqn.to_string(),
                ),
            };
            self.events.lock().unwrap().push(row);
        }
    }

    /// Spawn a server requiring DH-HMAC-CHAP auth, reading per-host
    /// secrets from `dhchap_path`. `audit` lets a test observe the
    /// success / refusal events the transport records.
    async fn spawn_server_with_dhchap(
        handler: Arc<StubHandler>,
        dhchap_path: std::path::PathBuf,
        audit: Arc<dyn LoginAuditSink>,
    ) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = Arc::clone(&handler) as Arc<dyn NvmeCommandHandler>;
        let regs = Arc::new(ControllerRegs::new());
        let aer = Arc::new(ControllerRegistry::new());
        tokio::spawn(async move {
            let _ = accept_loop(listener, h, regs, aer, None, None, Some(dhchap_path), audit).await;
        });
        port
    }

    /// Spawn a server sharing a caller-supplied `ControllerRegistry`, so
    /// the test can raise events the same way the dispatcher would.
    async fn spawn_server_with_aer(handler: Arc<StubHandler>, aer: Arc<ControllerRegistry>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = Arc::clone(&handler) as Arc<dyn NvmeCommandHandler>;
        let regs = Arc::new(ControllerRegs::new());
        tokio::spawn(async move {
            let _ = accept_loop(
                listener,
                h,
                regs,
                aer,
                None,
                None,
                None,
                Arc::new(NoopLoginAudit),
            )
            .await;
        });
        port
    }

    /// Spawn the server on `127.0.0.1:0` with an arbitrary boxed
    /// handler (the Discovery controller test uses a `DiscoveryHandler`
    /// rather than the `StubHandler`).
    async fn spawn_handler(h: Arc<dyn NvmeCommandHandler>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let regs = Arc::new(ControllerRegs::new());
        let aer = Arc::new(ControllerRegistry::new());
        tokio::spawn(async move {
            let _ = accept_loop(
                listener,
                h,
                regs,
                aer,
                None,
                None,
                None,
                Arc::new(NoopLoginAudit),
            )
            .await;
        });
        port
    }

    /// End-to-end Discovery controller: Connect to the well-known
    /// discovery NQN, Identify (expect CNTLTYPE = Discovery), then a
    /// full Get Log Page 0x70. Proves the `local_addr` thread-through —
    /// the I/O bind is wildcard (`io_traddr = None`), so the log entry's
    /// TRADDR must reflect the loopback address the connection landed on.
    #[tokio::test]
    async fn discovery_controller_lists_subsystem_with_reflected_traddr() {
        use nvme_base::identify::DISCOVERY_NQN;
        use nvme_base::log_page::{disc_sectype, disc_treq};
        use nvme_nvm::DiscoveryHandler;

        let handler: Arc<dyn NvmeCommandHandler> = Arc::new(DiscoveryHandler::new(
            "nqn.2025-10.com.metebalci:thurvsa".into(),
            4420,
            None, // wildcard I/O bind → reflect local_addr
            disc_sectype::NONE,
            disc_treq::NOT_REQUIRED,
            "TESTSN".into(),
            "ThurVSA Discovery".into(),
            "0.1.0".into(),
        ));
        let port = spawn_handler(handler).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await; // ICResp

        // Connect to the well-known discovery NQN on the admin queue.
        stream
            .write_all(&build_connect_pdu(DISCOVERY_NQN, 0))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let connect_resp = read_pdu_async(&mut stream).await;
        assert_eq!(connect_resp.header.pdu_type, pdu::PduType::CapsuleResp);

        // Identify Controller — CNTLTYPE at byte 111 must be 2 (Discovery).
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = AdminOpcode::Identify as u8;
        sqe[2] = 0x10; // CID
        sqe[40] = nvme_base::identify::CNS::Controller as u8;
        stream
            .write_all(&build_capsule_cmd_pdu(sqe, &[]))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let c2h = read_pdu_async(&mut stream).await;
        assert_eq!(c2h.header.pdu_type, pdu::PduType::C2HData);
        assert_eq!(c2h.body.len(), 16 + 4096);
        assert_eq!(c2h.body[16 + 111], 2, "CNTLTYPE = Discovery");

        // Get Log Page LID 0x70, full page: header(1024)+entry(1024) =
        // 2048 bytes → 512 dwords → NUMD zero-based 511.
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = AdminOpcode::GetLogPage as u8;
        sqe[2] = 0x11; // CID
        let cdw10 = u32::from(nvme_base::log_page::lid::DISCOVERY) | (511u32 << 16);
        sqe[40..44].copy_from_slice(&cdw10.to_le_bytes());
        stream
            .write_all(&build_capsule_cmd_pdu(sqe, &[]))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let c2h = read_pdu_async(&mut stream).await;
        assert_eq!(c2h.header.pdu_type, pdu::PduType::C2HData);
        assert_eq!(c2h.body.len(), 16 + 2048);
        // NUMREC @ data offset 8 = 1 subsystem.
        assert_eq!(
            u64::from_le_bytes(c2h.body[16 + 8..16 + 16].try_into().unwrap()),
            1
        );
        // Entry TRADDR @ data offset 1024+512 reflects the loopback addr
        // the discovery connection landed on (wildcard I/O bind).
        let traddr_off = 16 + 1024 + 512;
        assert_eq!(&c2h.body[traddr_off..traddr_off + 9], b"127.0.0.1");
        // TRSVCID @ data offset 1024+32 = the I/O port "4420".
        let trsvcid_off = 16 + 1024 + 32;
        assert_eq!(&c2h.body[trsvcid_off..trsvcid_off + 4], b"4420");
        // SUBNQN @ data offset 1024+256 = the referenced I/O subsystem.
        let subnqn_off = 16 + 1024 + 256;
        assert_eq!(
            &c2h.body[subnqn_off..subnqn_off + 33],
            b"nqn.2025-10.com.metebalci:thurvsa"
        );
    }

    /// An AER parked on the admin queue completes when a reservation
    /// event fires for the host, carrying the reservation-notice DW0
    /// (LID 0x80). Robust to park/notify ordering — a notify before the
    /// AER is parked queues the entry, and `park` then fires it.
    #[tokio::test]
    async fn async_event_request_completes_on_reservation_notice() {
        use nvme_nvm::{ReservationEvent, ReservationEventKind};
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let aer = Arc::new(ControllerRegistry::new());
        let port = spawn_server_with_aer(Arc::clone(&handler), Arc::clone(&aer)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await; // ICResp
        // Connect on the admin queue (qid 0); ConnectData hostid = [0xA1; 16].
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 0))
            .await
            .unwrap();
        let _ = read_pdu_async(&mut stream).await; // Connect response

        // Submit an AER (admin opcode 0x0C, CID 0x77). It parks.
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = AdminOpcode::AsyncEventRequest as u8;
        sqe[2..4].copy_from_slice(&0x77u16.to_le_bytes());
        stream
            .write_all(&build_capsule_cmd_pdu(sqe, &[]))
            .await
            .unwrap();
        stream.flush().await.unwrap();

        // Fire an event for this host (the dispatcher's notify path).
        aer.notify(ReservationEvent {
            host_id: [0xA1; 16],
            nsid: 1,
            kind: ReservationEventKind::RegistrationPreempted,
        });

        // The parked AER completes: CapsuleResp, CID 0x77, DW0 = LID 0x80
        // reservation notice, success status.
        let resp = read_pdu_async(&mut stream).await;
        assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
        let dw0 = u32::from_le_bytes([resp.body[0], resp.body[1], resp.body[2], resp.body[3]]);
        let cid = u16::from_le_bytes([resp.body[12], resp.body[13]]);
        let status = u16::from_le_bytes([resp.body[14], resp.body[15]]);
        assert_eq!(cid, 0x77);
        assert_eq!(dw0, 0x0080_0006);
        assert_eq!(status, 0, "AER completes with success");
    }

    /// Build an ICReq PDU.
    fn build_icreq_pdu() -> Vec<u8> {
        build_icreq_pdu_dgst(0)
    }

    /// Build an ICReq PDU requesting `dgst` (bit 0 header, bit 1 data).
    /// ICReq itself carries no digest — it's pre-negotiation.
    fn build_icreq_pdu_dgst(dgst: u8) -> Vec<u8> {
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
            dgst,
            maxr2t: 16,
        };
        icreq.write_to(&mut buf);
        buf
    }

    /// Build an Identify (admin) CapsuleCmd — the StubHandler answers it
    /// with a 4096-byte data-in payload, so it exercises the C2HData
    /// (controller->host) data digest path.
    fn build_identify_pdu(cid: u16) -> Vec<u8> {
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = AdminOpcode::Identify as u8;
        sqe[1] = 0b0100_0000; // PSDT = SglInline
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        build_capsule_cmd_pdu(sqe, &[])
    }

    /// Read one PDU and verify its digests against `cfg` (panicking on a
    /// mismatch) — the host-side mirror of the server's verify path.
    async fn read_pdu_verified(stream: &mut TcpStream, cfg: pdu::DigestCfg) -> pdu::RawPdu {
        let raw = pdu::RawPdu::read_async(stream).await.unwrap();
        raw.verify_header_digest(cfg)
            .expect("inbound header digest");
        raw.verify_data_digest(cfg).expect("inbound data digest");
        raw
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
    /// (override `subnqn` to test the mismatch case) and `CNTLID_ANY`
    /// requested (the right value for an admin-queue Connect). NVMe-oF
    /// Fabrics SQEs put FCTYPE at byte 4 (overlapping NSID); QID
    /// lives at CDW10[31:16] with RECFMT in [15:0] (we always
    /// write 0).
    fn build_connect_pdu(subnqn: &str, qid: u16) -> Vec<u8> {
        build_connect_pdu_cntlid(subnqn, qid, nvme_base::fabrics::CNTLID_ANY)
    }

    /// As [`build_connect_pdu`] but with an explicit requested CNTLID —
    /// an I/O-queue Connect names the CNTLID it received from its admin
    /// Connect.
    fn build_connect_pdu_cntlid(subnqn: &str, qid: u16, requested_cntlid: u16) -> Vec<u8> {
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
            requested_cntlid,
            subnqn: subnqn.to_string(),
            hostnqn: "nqn.2014-08.org.nvmexpress:uuid:test-host".to_string(),
        };
        let data = cd.to_bytes().unwrap();
        build_capsule_cmd_pdu(sqe, &data)
    }

    /// Default SUBNQN the test server answers for.
    const TEST_SUBNQN: &str = "nqn.2025-10.com.metebalci:thurvsa";

    /// Establish an NVMe-oF controller against the test server: an admin
    /// queue (QID 0, which mints the CNTLID) plus one attached I/O queue
    /// (QID 1, naming that CNTLID). Returns `(admin_stream, io_stream)`;
    /// the admin stream MUST be kept alive (the controller — and so the
    /// I/O queue's CNTLID — is freed when its last association drops).
    /// I/O-path tests run their commands on the returned `io_stream`.
    async fn connect_io_queue(port: u16) -> (TcpStream, TcpStream) {
        // Admin queue: creates the controller, returns its CNTLID in the
        // Connect Response DW0[15:0].
        let mut admin = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        admin.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut admin).await; // ICResp
        admin
            .write_all(&build_connect_pdu(TEST_SUBNQN, 0))
            .await
            .unwrap();
        let resp = read_pdu_async(&mut admin).await; // Connect resp
        let cntlid = u16::from_le_bytes([resp.body[0], resp.body[1]]);

        // I/O queue: attaches to that controller by CNTLID.
        let mut io = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        io.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut io).await; // ICResp
        io.write_all(&build_connect_pdu_cntlid(TEST_SUBNQN, 1, cntlid))
            .await
            .unwrap();
        let _ = read_pdu_async(&mut io).await; // Connect resp
        (admin, io)
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

    /// A second admin-queue Connect (fresh connection) is assigned a
    /// distinct CNTLID — `nvme list-ctrl` / multipath tooling no longer
    /// see every controller as ID 1.
    #[tokio::test]
    async fn admin_connect_assigns_distinct_cntlids() {
        let handler = StubHandler::new(TEST_SUBNQN);
        let port = spawn_server(Arc::clone(&handler)).await;

        let mut a = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        a.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut a).await;
        a.write_all(&build_connect_pdu(TEST_SUBNQN, 0))
            .await
            .unwrap();
        let ra = read_pdu_async(&mut a).await;
        assert_eq!(u16::from_le_bytes([ra.body[0], ra.body[1]]), 1);

        // `a` stays alive, so controller 1 is not freed before `b`
        // connects — `b` gets the next CNTLID.
        let mut b = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        b.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut b).await;
        b.write_all(&build_connect_pdu(TEST_SUBNQN, 0))
            .await
            .unwrap();
        let rb = read_pdu_async(&mut b).await;
        assert_eq!(u16::from_le_bytes([rb.body[0], rb.body[1]]), 2);
    }

    /// An I/O-queue Connect naming a CNTLID with no live controller is
    /// refused with Connect Invalid Parameters — the host must
    /// admin-Connect first to obtain a real CNTLID.
    #[tokio::test]
    async fn io_connect_unknown_cntlid_refused() {
        let handler = StubHandler::new(TEST_SUBNQN);
        let port = spawn_server(Arc::clone(&handler)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await; // ICResp
        // QID 1 naming CNTLID 7 — no controller has been created.
        stream
            .write_all(&build_connect_pdu_cntlid(TEST_SUBNQN, 1, 7))
            .await
            .unwrap();
        let resp = read_pdu_async(&mut stream).await;
        assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
        let status = u16::from_le_bytes([resp.body[14], resp.body[15]]);
        assert_eq!(status, StatusField::connect_invalid_parameters().to_u16());
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

    /// Issue #78: a host requesting header + data digests negotiates them
    /// in ICResp, and every subsequent PDU carries valid CRC32C digests
    /// in both directions — verified end to end through Connect + a
    /// data-bearing Identify.
    #[tokio::test]
    async fn digests_negotiated_end_to_end() {
        let port = spawn_server(StubHandler::new(TEST_SUBNQN)).await;
        let cfg = pdu::DigestCfg {
            header: true,
            data: true,
        };

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        // ICReq requests both digests (ICResp echoes the agreed set).
        stream.write_all(&build_icreq_pdu_dgst(0b11)).await.unwrap();
        stream.flush().await.unwrap();
        let icresp_pdu = read_pdu_async(&mut stream).await; // ICResp itself: no digest
        let icresp = pdu::ICResp::read_from(&icresp_pdu.body[..pdu::ICResp::PAYLOAD_LEN]).unwrap();
        assert_eq!(icresp.dgst, 0b11, "controller honored header+data digests");

        // Connect — now digested in both directions.
        stream
            .write_all(&pdu::apply_digests(build_connect_pdu(TEST_SUBNQN, 0), cfg))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let connect_resp = read_pdu_verified(&mut stream, cfg).await;
        assert_eq!(connect_resp.header.pdu_type, pdu::PduType::CapsuleResp);
        assert_eq!(
            connect_resp.header.flags & pdu::FLAGS_HDGSTF,
            pdu::FLAGS_HDGSTF
        );

        // Identify — 4096-byte data-in folds into one digested C2HData.
        stream
            .write_all(&pdu::apply_digests(build_identify_pdu(0x55), cfg))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let c2h = read_pdu_verified(&mut stream, cfg).await;
        assert_eq!(c2h.header.pdu_type, pdu::PduType::C2HData);
        assert_eq!(c2h.header.flags & pdu::FLAGS_HDGSTF, pdu::FLAGS_HDGSTF);
        assert_eq!(c2h.header.flags & pdu::FLAGS_DDGSTF, pdu::FLAGS_DDGSTF);
        let data = c2h.in_capsule_data().unwrap().unwrap();
        assert_eq!(data.len(), 4096);
        assert_eq!(data[0], 0xC0, "stub canary survived digest framing");
    }

    /// A corrupted inbound header digest is fatal: C2HTermReq with FES =
    /// Header Digest Error (0x03), then the connection closes.
    #[tokio::test]
    async fn header_digest_error_is_fatal() {
        let port = spawn_server(StubHandler::new(TEST_SUBNQN)).await;
        let cfg = pdu::DigestCfg {
            header: true,
            data: false,
        };

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu_dgst(0b01)).await.unwrap(); // header only
        stream.flush().await.unwrap();
        let _ = read_pdu_async(&mut stream).await; // ICResp

        // Flip a byte inside the CRC'd header region (the SQE opcode at
        // PDU offset 8) AFTER the digest was computed -> stored CRC stale.
        let mut connect = pdu::apply_digests(build_connect_pdu(TEST_SUBNQN, 0), cfg);
        connect[8] ^= 0xFF;
        stream.write_all(&connect).await.unwrap();
        stream.flush().await.unwrap();

        let term = read_pdu_async(&mut stream).await;
        assert_eq!(term.header.pdu_type, pdu::PduType::C2HTermReq);
        assert_eq!(&term.body[0..2], &0x0003u16.to_le_bytes()); // Header Digest Error
        let mut tmp = [0u8; 1];
        let n = timeout(Duration::from_secs(2), stream.read(&mut tmp))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 0, "connection closed after fatal header digest error");
    }

    /// A corrupted inbound data digest fails just that command (Data
    /// Transfer Error) and leaves the connection usable — NOT a fatal
    /// teardown (NVMe/TCP §3.4, no FES for data digest).
    #[tokio::test]
    async fn data_digest_error_fails_command_not_connection() {
        let port = spawn_server(StubHandler::new(TEST_SUBNQN)).await;
        let cfg = pdu::DigestCfg {
            header: true,
            data: true,
        };

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu_dgst(0b11)).await.unwrap();
        stream.flush().await.unwrap();
        let _ = read_pdu_async(&mut stream).await; // ICResp
        stream
            .write_all(&pdu::apply_digests(build_connect_pdu(TEST_SUBNQN, 0), cfg))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let _ = read_pdu_verified(&mut stream, cfg).await; // Connect resp

        // Write carrying 64 bytes in-capsule; corrupt one data byte after
        // the digests are computed. Header digest still verifies (data
        // begins at PDU offset 76 = HLEN 72 + 4-byte header digest), so
        // only the data digest fails.
        let mut cmd = pdu::apply_digests(build_write_with_icd_pdu(0x33, 1, &[0x11; 64]), cfg);
        cmd[76] ^= 0xFF;
        stream.write_all(&cmd).await.unwrap();
        stream.flush().await.unwrap();

        let resp = read_pdu_verified(&mut stream, cfg).await;
        assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
        let status = u16::from_le_bytes([resp.body[14], resp.body[15]]);
        assert_eq!(status, StatusField::data_transfer_error().to_u16());

        // Connection survives: a follow-up Identify still round-trips.
        stream
            .write_all(&pdu::apply_digests(build_identify_pdu(0x44), cfg))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let c2h = read_pdu_verified(&mut stream, cfg).await;
        assert_eq!(c2h.header.pdu_type, pdu::PduType::C2HData);
    }

    /// Build a Write CapsuleCmd carrying its full payload in-capsule
    /// (SGL length == ICD length, so no R2T) — used to exercise the
    /// in-capsule data digest path.
    fn build_write_with_icd_pdu(cid: u16, nsid: u32, data: &[u8]) -> Vec<u8> {
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = NvmOpcode::Write as u8;
        sqe[1] = 0b0100_0000; // PSDT = SglInline
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
        sqe[24 + 8..24 + 12].copy_from_slice(&(data.len() as u32).to_le_bytes());
        build_capsule_cmd_pdu(sqe, data)
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

        let (_admin, mut stream) = connect_io_queue(port).await;

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
    async fn oversized_transfer_length_rejected_without_alloc() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;

        let (_admin, mut stream) = connect_io_queue(port).await;

        // A Write declaring a 4 GiB-1 transfer length. Pre-fix this
        // flowed into `vec![0u8; sgl_len]`. It must instead come back
        // as a failure CQE (Invalid Field), with no R2T and no handler
        // dispatch.
        let cid: u16 = 0x55;
        stream
            .write_all(&build_write_no_icd_pdu(cid, 1, u32::MAX))
            .await
            .unwrap();
        stream.flush().await.unwrap();

        let resp = read_pdu_async(&mut stream).await;
        assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
        assert_eq!(&resp.body[12..14], &cid.to_le_bytes());
        let status = u16::from_le_bytes([resp.body[14], resp.body[15]]);
        assert_eq!(status, StatusField::invalid_field().to_u16());
        assert_eq!(handler.io_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn write_with_partial_icd_uses_r2t_for_tail() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;

        let (_admin, mut stream) = connect_io_queue(port).await;

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

        let (_admin, mut stream) = connect_io_queue(port).await;

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

        let (_admin, mut stream) = connect_io_queue(port).await;

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

    /// Issue #56: a clean connection teardown must free the controller's
    /// CNTLID in the *shared* registry (`serve_connection` ->
    /// `aer.disconnect`), so a later association reuses it. The
    /// registry's own unit test covers the method in isolation; this
    /// proves the transport actually invokes it. Dropping that call
    /// leaks a CNTLID on every clean Disconnect and passes every other
    /// transport test.
    #[tokio::test]
    async fn disconnect_frees_cntlid_for_reuse() {
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let aer = Arc::new(ControllerRegistry::new());
        let port = spawn_server_with_aer(Arc::clone(&handler), Arc::clone(&aer)).await;
        let host = [0xA1u8; 16]; // build_connect_pdu's ConnectData hostid

        // Admin Connect -> CNTLID 1, registered in the shared registry.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut stream).await;
        stream
            .write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 0))
            .await
            .unwrap();
        let resp = read_pdu_async(&mut stream).await;
        assert_eq!(
            u16::from_le_bytes([resp.body[0], resp.body[1]]),
            1,
            "first controller gets CNTLID 1",
        );
        assert_eq!(
            aer.cntlids_for_host(host),
            vec![1],
            "controller registered in the shared registry",
        );

        // Tear the connection down and wait for the socket to close.
        stream.write_all(&build_disconnect_pdu(0xDD)).await.unwrap();
        let _ = read_pdu_async(&mut stream).await; // Disconnect response
        let mut tmp = [0u8; 1];
        let n = timeout(Duration::from_secs(2), stream.read(&mut tmp))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 0, "server closes after Disconnect");

        // The CNTLID is freed by the transport teardown, which runs
        // after the socket close — poll the shared registry briefly.
        let mut freed = false;
        for _ in 0..40 {
            if aer.cntlids_for_host(host).is_empty() {
                freed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(freed, "teardown must free the CNTLID (aer.disconnect)");

        // And it is reused: a fresh admin Connect gets CNTLID 1 again.
        let mut s2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s2.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut s2).await;
        s2.write_all(&build_connect_pdu("nqn.2025-10.com.metebalci:thurvsa", 0))
            .await
            .unwrap();
        let resp2 = read_pdu_async(&mut s2).await;
        assert_eq!(
            u16::from_le_bytes([resp2.body[0], resp2.body[1]]),
            1,
            "CNTLID 1 reused after teardown",
        );
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

        let (_admin, mut stream) = connect_io_queue(port).await;

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

    #[tokio::test]
    async fn io_queue_steady_state_cqe_echoes_qid_in_sqid() {
        // Steady-state completions must carry SQID = QID, matching the
        // Connect Response + auth phases (issue #72). A Flush on QID 1
        // returns a no-payload success: data_in is empty, so the writer
        // emits a standalone CapsuleResp (no SUCCESS-bit fold), making the
        // SQID field observable on the wire.
        let handler = StubHandler::new("nqn.2025-10.com.metebalci:thurvsa");
        let port = spawn_server(Arc::clone(&handler)).await;

        let (_admin, mut stream) = connect_io_queue(port).await;

        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = NvmOpcode::Flush as u8;
        sqe[2] = 0x77; // CID
        sqe[4] = 0x01; // NSID
        stream
            .write_all(&build_capsule_cmd_pdu(sqe, &[]))
            .await
            .unwrap();
        let resp = read_pdu_async(&mut stream).await;
        assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
        assert_eq!(
            u16::from_le_bytes([resp.body[10], resp.body[11]]),
            1,
            "steady-state CapsuleResp must echo QID in SQID",
        );
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
        let (_admin, mut stream) = connect_io_queue(port).await;

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
        let (_admin, mut stream) = connect_io_queue(port).await;

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
        let (_admin, mut stream) = connect_io_queue(port).await;

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
        let (_admin, mut stream) = connect_io_queue(port).await;

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
        let (_admin, mut stream) = connect_io_queue(port).await;

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

    // ===================== DH-HMAC-CHAP auth phase =====================

    const TEST_HOSTNQN: &str = "nqn.2014-08.org.nvmexpress:uuid:test-host";

    fn write_dhchap_file(slug: &str, entry: crate::identity::DhchapEntry) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "nvme-tcp-dhchap-srv-{}-{}.json",
            slug,
            std::process::id()
        ));
        crate::identity::NvmetcpDhchapFile {
            version: 1,
            dhchap: vec![entry],
        }
        .save(&path)
        .unwrap();
        path
    }

    fn dhchap_entry_for(
        secret: &str,
        ctrl: Option<&str>,
        volumes: &[&str],
    ) -> crate::identity::DhchapEntry {
        crate::identity::DhchapEntry {
            host_nqn: TEST_HOSTNQN.into(),
            dhchap_key: secret.into(),
            dhchap_ctrl_key: ctrl.map(String::from),
            disabled: false,
            volumes: Some(volumes.iter().map(|s| s.to_string()).collect()),
            previous_dhchap_key: None,
            previous_expires_at: None,
        }
    }

    fn build_auth_send_pdu(cid: u16, msg: &[u8]) -> Vec<u8> {
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = AdminOpcode::Fabrics as u8;
        sqe[1] = 0b0100_0000; // PSDT SglInline
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4] = FabricsType::AuthenticationSend as u8;
        // CDW10 = [resv3, spsp0, spsp1, secp].
        sqe[41] = 0x01;
        sqe[42] = 0x01;
        sqe[43] = dhwire::NVME_AUTH_DHCHAP_PROTOCOL_IDENTIFIER;
        sqe[44..48].copy_from_slice(&(msg.len() as u32).to_le_bytes()); // TL
        build_capsule_cmd_pdu(sqe, msg)
    }

    fn build_auth_receive_pdu(cid: u16, al: u32) -> Vec<u8> {
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = AdminOpcode::Fabrics as u8;
        sqe[1] = 0b0100_0000;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4] = FabricsType::AuthenticationReceive as u8;
        sqe[41] = 0x01;
        sqe[42] = 0x01;
        sqe[43] = dhwire::NVME_AUTH_DHCHAP_PROTOCOL_IDENTIFIER;
        sqe[44..48].copy_from_slice(&al.to_le_bytes()); // AL
        build_capsule_cmd_pdu(sqe, &[])
    }

    fn host_negotiate(t_id: u16, hashes: &[u8], groups: &[u8]) -> Vec<u8> {
        dhwire::build_negotiate(
            t_id,
            0,
            &[dhwire::ProtocolDescriptor {
                authid: dhwire::NVME_AUTH_DHCHAP_AUTH_ID,
                hash_ids: hashes.to_vec(),
                dhgroup_ids: groups.to_vec(),
            }],
        )
    }

    /// Read a controller->host auth message (C2HData + CapsuleResp);
    /// returns the message bytes.
    async fn read_auth_message(stream: &mut TcpStream) -> Vec<u8> {
        let c2h = read_pdu_async(stream).await;
        assert_eq!(c2h.header.pdu_type, pdu::PduType::C2HData);
        let msg = c2h.body[16..].to_vec();
        let resp = read_pdu_async(stream).await;
        assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
        msg
    }

    async fn read_capsule_resp_ack(stream: &mut TcpStream) {
        let r = read_pdu_async(stream).await;
        assert_eq!(r.header.pdu_type, pdu::PduType::CapsuleResp);
    }

    /// Compute a host Reply from a received Challenge, mirroring the
    /// controller's crypto. Returns the Reply bytes plus, for mutual
    /// auth, the R2 the controller is expected to return in Success1.
    fn host_build_reply(
        challenge_msg: &[u8],
        secret: &str,
        ctrl_secret: Option<&str>,
    ) -> (Vec<u8>, Option<Vec<u8>>) {
        let ch = dhwire::parse_challenge(challenge_msg).unwrap();
        let (host_dh_value, session_key) = if ch.dhgid != dhwire::NVME_AUTH_DHGROUP_NULL {
            let kp = dhcrypt::DhKeypair::generate(ch.dhgid).unwrap();
            let sk = kp.session_key(&ch.dh_value, ch.hashid).unwrap();
            (kp.public_value().unwrap(), Some(sk))
        } else {
            (Vec::new(), None)
        };
        let augment = |c: &[u8]| match &session_key {
            Some(sk) => dhcrypt::augmented_challenge(ch.hashid, sk, c).unwrap(),
            None => c.to_vec(),
        };
        let key = dhcrypt::parse_dhchap_secret(secret).unwrap();
        let tk = dhcrypt::transform_key(&key, TEST_HOSTNQN).unwrap();
        let r1 = dhcrypt::dhchap_response(&dhcrypt::ResponseInput {
            transformed_key: &tk,
            hash_id: ch.hashid,
            challenge: &augment(&ch.cval),
            seqnum: ch.seqnum,
            t_id: ch.t_id,
            sc_c: 0,
            label: dhcrypt::LABEL_HOST,
            nqn_first: TEST_HOSTNQN,
            nqn_second: TEST_SUBNQN,
        })
        .unwrap();
        let (c2_opt, s2, expected_r2) = if let Some(cs) = ctrl_secret {
            let c2 = vec![0x5cu8; ch.cval.len()];
            let s2 = 0x0807_0605u32;
            let ck = dhcrypt::parse_dhchap_secret(cs).unwrap();
            let ctk = dhcrypt::transform_key(&ck, TEST_SUBNQN).unwrap();
            let r2 = dhcrypt::dhchap_response(&dhcrypt::ResponseInput {
                transformed_key: &ctk,
                hash_id: ch.hashid,
                challenge: &augment(&c2),
                seqnum: s2,
                t_id: ch.t_id,
                sc_c: 0,
                label: dhcrypt::LABEL_CONTROLLER,
                nqn_first: TEST_SUBNQN,
                nqn_second: TEST_HOSTNQN,
            })
            .unwrap();
            (Some(c2), s2, Some(r2))
        } else {
            (None, 0u32, None)
        };
        let msg = dhwire::build_reply(ch.t_id, &r1, c2_opt.as_deref(), s2, &host_dh_value);
        (msg, expected_r2)
    }

    /// Drive ICReq + admin Connect, asserting AUTHREQ is set. Leaves the
    /// stream positioned to start the auth exchange.
    async fn connect_with_authreq(port: u16) -> TcpStream {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut s).await; // ICResp
        s.write_all(&build_connect_pdu(TEST_SUBNQN, 0))
            .await
            .unwrap();
        let resp = read_pdu_async(&mut s).await;
        let dw0 = u32::from_le_bytes([resp.body[0], resp.body[1], resp.body[2], resp.body[3]]);
        // ATR (Authentication Transaction Required) is DW0 bit 17.
        assert_eq!((dw0 >> 17) & 1, 1, "ATR must be set when dhchap on");
        s
    }

    #[tokio::test]
    async fn dhchap_null_group_happy_path_then_admin() {
        let secret = dhcrypt::encode_dhchap_secret(&[0x11; 32], 0);
        let path = write_dhchap_file("null-ok", dhchap_entry_for(&secret, None, &["vol-a"]));
        let handler = StubHandler::new(TEST_SUBNQN);
        let audit = Arc::new(CapturingAudit::default());
        let port =
            spawn_server_with_dhchap(Arc::clone(&handler), path.clone(), Arc::clone(&audit) as _)
                .await;

        let mut s = connect_with_authreq(port).await;
        // Negotiate (Auth Send) -> ACK.
        s.write_all(&build_auth_send_pdu(
            0x10,
            &host_negotiate(
                0x77,
                &[dhwire::NVME_AUTH_HASH_SHA256],
                &[dhwire::NVME_AUTH_DHGROUP_NULL],
            ),
        ))
        .await
        .unwrap();
        read_capsule_resp_ack(&mut s).await;
        // Auth Receive -> Challenge.
        s.write_all(&build_auth_receive_pdu(0x11, 4096))
            .await
            .unwrap();
        let challenge = read_auth_message(&mut s).await;
        // Reply (Auth Send) -> ACK.
        let (reply, _) = host_build_reply(&challenge, &secret, None);
        s.write_all(&build_auth_send_pdu(0x12, &reply))
            .await
            .unwrap();
        read_capsule_resp_ack(&mut s).await;
        // Auth Receive -> Success1 (unidirectional, no R2).
        s.write_all(&build_auth_receive_pdu(0x13, 4096))
            .await
            .unwrap();
        let success1 = read_auth_message(&mut s).await;
        let s1 = dhwire::parse_success1(&success1).unwrap();
        assert_eq!(s1.response, None);
        // Success2 (Auth Send) -> ACK.
        s.write_all(&build_auth_send_pdu(0x14, &dhwire::build_success2(0x77)))
            .await
            .unwrap();
        read_capsule_resp_ack(&mut s).await;

        // Auth complete: a normal admin Identify now succeeds.
        let mut sqe = [0u8; nvme_base::SQE_SIZE];
        sqe[0] = AdminOpcode::Identify as u8;
        sqe[2] = 0x55;
        sqe[40] = nvme_base::identify::CNS::Controller as u8;
        s.write_all(&build_capsule_cmd_pdu(sqe, &[])).await.unwrap();
        let c2h = read_pdu_async(&mut s).await;
        assert_eq!(c2h.header.pdu_type, pdu::PduType::C2HData);
        assert_eq!(c2h.body[16], 0xC0); // stub canary -> auth gate passed

        // The transport must have recorded a DhchapSuccess for this host
        // (guards the success-path audit emit in serve_connection).
        let mut recorded = None;
        for _ in 0..50 {
            if let Some(row) = audit.events.lock().unwrap().first().cloned() {
                recorded = Some(row);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (kind, reason, host) = recorded.expect("a DhchapSuccess must be recorded");
        assert_eq!(kind, "success");
        assert_eq!(reason, "");
        assert_eq!(host, TEST_HOSTNQN);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn dhchap_wrong_secret_refused() {
        let real = dhcrypt::encode_dhchap_secret(&[0x22; 32], 0);
        let wrong = dhcrypt::encode_dhchap_secret(&[0x99; 32], 0);
        let path = write_dhchap_file("wrong", dhchap_entry_for(&real, None, &["vol-a"]));
        let handler = StubHandler::new(TEST_SUBNQN);
        let audit = Arc::new(CapturingAudit::default());
        let port =
            spawn_server_with_dhchap(Arc::clone(&handler), path.clone(), Arc::clone(&audit) as _)
                .await;

        let mut s = connect_with_authreq(port).await;
        s.write_all(&build_auth_send_pdu(
            0x10,
            &host_negotiate(
                0x33,
                &[dhwire::NVME_AUTH_HASH_SHA256],
                &[dhwire::NVME_AUTH_DHGROUP_NULL],
            ),
        ))
        .await
        .unwrap();
        read_capsule_resp_ack(&mut s).await;
        s.write_all(&build_auth_receive_pdu(0x11, 4096))
            .await
            .unwrap();
        let challenge = read_auth_message(&mut s).await;
        // Reply computed with the WRONG secret.
        let (reply, _) = host_build_reply(&challenge, &wrong, None);
        s.write_all(&build_auth_send_pdu(0x12, &reply))
            .await
            .unwrap();
        read_capsule_resp_ack(&mut s).await;
        // Auth Receive yields a Failure1, then the connection closes.
        s.write_all(&build_auth_receive_pdu(0x13, 4096))
            .await
            .unwrap();
        let msg = read_auth_message(&mut s).await;
        let f = dhwire::parse_failure(&msg).unwrap();
        assert_eq!(f.rescode, dhwire::NVME_AUTH_DHCHAP_FAILURE_REASON_FAILED);
        assert_eq!(f.rescode_exp, dhwire::NVME_AUTH_DHCHAP_FAILURE_FAILED);
        // The refusal must have driven a `reply_invalid` audit/alert
        // event (the security-observability gap issue #68 closes). Poll
        // briefly: the transport records it on its own task right before
        // closing the connection, which may land just after the host
        // observes the Failure1.
        let mut recorded = None;
        for _ in 0..50 {
            if let Some(row) = audit.events.lock().unwrap().first().cloned() {
                recorded = Some(row);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (kind, reason, host) = recorded.expect("a DhchapFailure must be recorded");
        assert_eq!(kind, "failure");
        assert_eq!(reason, "reply_invalid");
        assert_eq!(host, TEST_HOSTNQN);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn dhchap_unknown_host_refused_at_negotiation() {
        // The secret store has an entry, but for a *different* host NQN,
        // so the lookup for our connecting host misses. This is the most
        // common production refusal (an un-provisioned host) and exits at
        // the negotiation stage with reason `negotiation_failed`.
        let other = crate::identity::DhchapEntry {
            host_nqn: "nqn.2014-08.org.nvmexpress:uuid:someone-else".into(),
            dhchap_key: dhcrypt::encode_dhchap_secret(&[0x55; 32], 0),
            dhchap_ctrl_key: None,
            disabled: false,
            volumes: Some(vec!["vol-a".into()]),
            previous_dhchap_key: None,
            previous_expires_at: None,
        };
        let path = write_dhchap_file("unknown-host", other);
        let handler = StubHandler::new(TEST_SUBNQN);
        let audit = Arc::new(CapturingAudit::default());
        let port =
            spawn_server_with_dhchap(Arc::clone(&handler), path.clone(), Arc::clone(&audit) as _)
                .await;

        let mut s = connect_with_authreq(port).await;
        // Negotiate (Auth Send) -> ACK.
        s.write_all(&build_auth_send_pdu(
            0x10,
            &host_negotiate(
                0x44,
                &[dhwire::NVME_AUTH_HASH_SHA256],
                &[dhwire::NVME_AUTH_DHGROUP_NULL],
            ),
        ))
        .await
        .unwrap();
        read_capsule_resp_ack(&mut s).await;
        // Auth Receive -> the negotiation outcome is already a failure
        // (host unknown), so the controller answers with Failure1 instead
        // of a Challenge and closes.
        s.write_all(&build_auth_receive_pdu(0x11, 4096))
            .await
            .unwrap();
        let msg = read_auth_message(&mut s).await;
        let f = dhwire::parse_failure(&msg).unwrap();
        assert_eq!(f.rescode, dhwire::NVME_AUTH_DHCHAP_FAILURE_REASON_FAILED);
        assert_eq!(f.rescode_exp, dhwire::NVME_AUTH_DHCHAP_FAILURE_FAILED);

        let mut recorded = None;
        for _ in 0..50 {
            if let Some(row) = audit.events.lock().unwrap().first().cloned() {
                recorded = Some(row);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (kind, reason, host) = recorded.expect("a DhchapFailure must be recorded");
        assert_eq!(kind, "failure");
        assert_eq!(reason, "negotiation_failed");
        assert_eq!(host, TEST_HOSTNQN);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn dhchap_bidirectional_mutual_auth() {
        let host_secret = dhcrypt::encode_dhchap_secret(&[0x33; 48], 0);
        let ctrl_secret = dhcrypt::encode_dhchap_secret(&[0x44; 48], 0);
        let path = write_dhchap_file(
            "mutual",
            dhchap_entry_for(&host_secret, Some(&ctrl_secret), &["vol-a"]),
        );
        let handler = StubHandler::new(TEST_SUBNQN);
        let port =
            spawn_server_with_dhchap(Arc::clone(&handler), path.clone(), Arc::new(NoopLoginAudit))
                .await;

        let mut s = connect_with_authreq(port).await;
        s.write_all(&build_auth_send_pdu(
            0x10,
            &host_negotiate(
                0x44,
                &[dhwire::NVME_AUTH_HASH_SHA384],
                &[dhwire::NVME_AUTH_DHGROUP_NULL],
            ),
        ))
        .await
        .unwrap();
        read_capsule_resp_ack(&mut s).await;
        s.write_all(&build_auth_receive_pdu(0x11, 4096))
            .await
            .unwrap();
        let challenge = read_auth_message(&mut s).await;
        let (reply, expected_r2) = host_build_reply(&challenge, &host_secret, Some(&ctrl_secret));
        s.write_all(&build_auth_send_pdu(0x12, &reply))
            .await
            .unwrap();
        read_capsule_resp_ack(&mut s).await;
        s.write_all(&build_auth_receive_pdu(0x13, 4096))
            .await
            .unwrap();
        let success1 = read_auth_message(&mut s).await;
        let s1 = dhwire::parse_success1(&success1).unwrap();
        // Controller proved itself: Success1 carries R2 matching ours.
        assert_eq!(s1.response, expected_r2);
        s.write_all(&build_auth_send_pdu(0x14, &dhwire::build_success2(0x44)))
            .await
            .unwrap();
        read_capsule_resp_ack(&mut s).await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn dhchap_ffdhe2048_full_dh() {
        let secret = dhcrypt::encode_dhchap_secret(&[0x55; 32], 1); // SHA-256 transform
        let path = write_dhchap_file("ffdhe", dhchap_entry_for(&secret, None, &["vol-a"]));
        let handler = StubHandler::new(TEST_SUBNQN);
        let port =
            spawn_server_with_dhchap(Arc::clone(&handler), path.clone(), Arc::new(NoopLoginAudit))
                .await;

        let mut s = connect_with_authreq(port).await;
        // Offer NULL + ffdhe2048; the controller prefers ffdhe2048.
        s.write_all(&build_auth_send_pdu(
            0x10,
            &host_negotiate(
                0x55,
                &[dhwire::NVME_AUTH_HASH_SHA256],
                &[
                    dhwire::NVME_AUTH_DHGROUP_NULL,
                    dhwire::NVME_AUTH_DHGROUP_2048,
                ],
            ),
        ))
        .await
        .unwrap();
        read_capsule_resp_ack(&mut s).await;
        s.write_all(&build_auth_receive_pdu(0x11, 4096))
            .await
            .unwrap();
        let challenge = read_auth_message(&mut s).await;
        // The controller must have selected the FFDHE group.
        let ch = dhwire::parse_challenge(&challenge).unwrap();
        assert_eq!(ch.dhgid, dhwire::NVME_AUTH_DHGROUP_2048);
        assert_eq!(ch.dh_value.len(), 256);
        let (reply, _) = host_build_reply(&challenge, &secret, None);
        s.write_all(&build_auth_send_pdu(0x12, &reply))
            .await
            .unwrap();
        read_capsule_resp_ack(&mut s).await;
        s.write_all(&build_auth_receive_pdu(0x13, 4096))
            .await
            .unwrap();
        let success1 = read_auth_message(&mut s).await;
        assert!(dhwire::parse_success1(&success1).is_ok());
        s.write_all(&build_auth_send_pdu(0x14, &dhwire::build_success2(0x55)))
            .await
            .unwrap();
        read_capsule_resp_ack(&mut s).await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn dhchap_challenge_exceeding_al_fails_command() {
        // The host advertises an Allocation Length on the Authentication
        // Receive too small to hold the Challenge. Rather than over-send a
        // C2HData, the controller fails the command with Invalid Field in
        // Command (issue #71) and closes the connection.
        let secret = dhcrypt::encode_dhchap_secret(&[0x11; 32], 0);
        let path = write_dhchap_file("al-too-small", dhchap_entry_for(&secret, None, &["vol-a"]));
        let handler = StubHandler::new(TEST_SUBNQN);
        let port =
            spawn_server_with_dhchap(Arc::clone(&handler), path.clone(), Arc::new(NoopLoginAudit))
                .await;

        let mut s = connect_with_authreq(port).await;
        s.write_all(&build_auth_send_pdu(
            0x10,
            &host_negotiate(
                0x77,
                &[dhwire::NVME_AUTH_HASH_SHA256],
                &[dhwire::NVME_AUTH_DHGROUP_NULL],
            ),
        ))
        .await
        .unwrap();
        read_capsule_resp_ack(&mut s).await;
        // A SHA-256 NULL-group Challenge is 16 + 32 = 48 bytes; AL = 16
        // cannot hold it.
        s.write_all(&build_auth_receive_pdu(0x11, 16))
            .await
            .unwrap();
        let resp = read_pdu_async(&mut s).await;
        assert_eq!(
            resp.header.pdu_type,
            pdu::PduType::CapsuleResp,
            "AL-too-small must yield a bare CapsuleResp, not a C2HData",
        );
        let status = u16::from_le_bytes([resp.body[14], resp.body[15]]);
        assert_eq!(
            status,
            StatusField::invalid_field().to_u16(),
            "over-sized auth message must fail with Invalid Field in Command",
        );
        // CID echoed.
        assert_eq!(u16::from_le_bytes([resp.body[12], resp.body[13]]), 0x11);
        // The auth phase bailed, so the controller closes the connection:
        // the next read sees EOF, not a trailing C2HData (which would mean
        // the controller over-sent the Challenge after the failure CQE).
        let mut probe = [0u8; 1];
        let n = s.read(&mut probe).await.unwrap();
        assert_eq!(
            n, 0,
            "controller must close the connection after an AL-too-small failure",
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn dhchap_reply_invalid_with_tiny_al_still_audits() {
        // A genuine credential refusal (wrong secret) where the host set a
        // too-small AL on the failure-pulling Authentication Receive: the
        // 8-byte Failure1 can't be delivered (the command fails with
        // Invalid Field), but the refusal MUST still be audited + counted
        // for brute-force detection. Guards the issue #71 review fix that
        // emits the failure event *before* attempting the Failure1 send.
        let real = dhcrypt::encode_dhchap_secret(&[0x22; 32], 0);
        let wrong = dhcrypt::encode_dhchap_secret(&[0x99; 32], 0);
        let path = write_dhchap_file("tiny-al-audit", dhchap_entry_for(&real, None, &["vol-a"]));
        let handler = StubHandler::new(TEST_SUBNQN);
        let audit = Arc::new(CapturingAudit::default());
        let port =
            spawn_server_with_dhchap(Arc::clone(&handler), path.clone(), Arc::clone(&audit) as _)
                .await;

        let mut s = connect_with_authreq(port).await;
        s.write_all(&build_auth_send_pdu(
            0x10,
            &host_negotiate(
                0x33,
                &[dhwire::NVME_AUTH_HASH_SHA256],
                &[dhwire::NVME_AUTH_DHGROUP_NULL],
            ),
        ))
        .await
        .unwrap();
        read_capsule_resp_ack(&mut s).await;
        s.write_all(&build_auth_receive_pdu(0x11, 4096))
            .await
            .unwrap();
        let challenge = read_auth_message(&mut s).await;
        let (reply, _) = host_build_reply(&challenge, &wrong, None);
        s.write_all(&build_auth_send_pdu(0x12, &reply))
            .await
            .unwrap();
        read_capsule_resp_ack(&mut s).await;
        // Failure-pulling Auth Receive with AL = 4 < the 8-byte Failure1.
        s.write_all(&build_auth_receive_pdu(0x13, 4)).await.unwrap();
        let resp = read_pdu_async(&mut s).await;
        assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
        assert_eq!(
            u16::from_le_bytes([resp.body[14], resp.body[15]]),
            StatusField::invalid_field().to_u16(),
            "too-small AL on the Failure1 must fail the command",
        );
        // Despite the undeliverable Failure1, the refusal is still audited.
        let mut recorded = None;
        for _ in 0..50 {
            if let Some(row) = audit.events.lock().unwrap().first().cloned() {
                recorded = Some(row);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (kind, reason, host) =
            recorded.expect("a DhchapFailure must be recorded even when the AL fails the send");
        assert_eq!(kind, "failure");
        assert_eq!(reason, "reply_invalid");
        assert_eq!(host, TEST_HOSTNQN);
        let _ = std::fs::remove_file(&path);
    }

    /// Admin Connect (QID 0, mints the CNTLID) then an I/O Connect (QID 1
    /// naming that CNTLID) against a dhchap-enabled server, asserting ATR
    /// on the I/O Connect Response and that its SQID echoes QID 1. The
    /// admin auth is left unstarted; its stream is returned only to keep
    /// the controller (and so the CNTLID) alive. Returns
    /// `(admin_stream, io_stream)` positioned to auth on the I/O queue.
    async fn connect_io_with_authreq(port: u16) -> (TcpStream, TcpStream) {
        let mut admin = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        admin.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut admin).await; // ICResp
        admin
            .write_all(&build_connect_pdu(TEST_SUBNQN, 0))
            .await
            .unwrap();
        let resp = read_pdu_async(&mut admin).await;
        let cntlid = u16::from_le_bytes([resp.body[0], resp.body[1]]);

        let mut io = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        io.write_all(&build_icreq_pdu()).await.unwrap();
        let _ = read_pdu_async(&mut io).await; // ICResp
        io.write_all(&build_connect_pdu_cntlid(TEST_SUBNQN, 1, cntlid))
            .await
            .unwrap();
        let resp = read_pdu_async(&mut io).await;
        let dw0 = u32::from_le_bytes([resp.body[0], resp.body[1], resp.body[2], resp.body[3]]);
        assert_eq!((dw0 >> 17) & 1, 1, "ATR must be set on the I/O queue too");
        assert_eq!(
            u16::from_le_bytes([resp.body[10], resp.body[11]]),
            1,
            "I/O Connect Response SQID echoes QID 1",
        );
        (admin, io)
    }

    #[tokio::test]
    async fn dhchap_io_queue_auth_echoes_qid_in_sqid() {
        // On an I/O-queue (QID > 0) auth, every auth-phase CapsuleResp must
        // echo the queue id in SQID, matching the Connect Response (issue
        // #71). On the admin queue QID is 0, so the echo is only observable
        // on an I/O queue.
        let secret = dhcrypt::encode_dhchap_secret(&[0x21; 32], 0);
        let path = write_dhchap_file("io-qid-echo", dhchap_entry_for(&secret, None, &["vol-a"]));
        let handler = StubHandler::new(TEST_SUBNQN);
        let port =
            spawn_server_with_dhchap(Arc::clone(&handler), path.clone(), Arc::new(NoopLoginAudit))
                .await;

        let (_admin, mut s) = connect_io_with_authreq(port).await;

        // Negotiate (Auth Send) -> ACK; the ACK CapsuleResp echoes QID 1.
        s.write_all(&build_auth_send_pdu(
            0x10,
            &host_negotiate(
                0x55,
                &[dhwire::NVME_AUTH_HASH_SHA256],
                &[dhwire::NVME_AUTH_DHGROUP_NULL],
            ),
        ))
        .await
        .unwrap();
        let ack = read_pdu_async(&mut s).await;
        assert_eq!(ack.header.pdu_type, pdu::PduType::CapsuleResp);
        assert_eq!(
            u16::from_le_bytes([ack.body[10], ack.body[11]]),
            1,
            "auth-send ACK must echo QID in SQID",
        );

        // Auth Receive -> Challenge; the trailing CapsuleResp echoes QID 1.
        s.write_all(&build_auth_receive_pdu(0x11, 4096))
            .await
            .unwrap();
        let c2h = read_pdu_async(&mut s).await;
        assert_eq!(c2h.header.pdu_type, pdu::PduType::C2HData);
        let challenge = c2h.body[16..].to_vec();
        let resp = read_pdu_async(&mut s).await;
        assert_eq!(resp.header.pdu_type, pdu::PduType::CapsuleResp);
        assert_eq!(
            u16::from_le_bytes([resp.body[10], resp.body[11]]),
            1,
            "auth-receive CapsuleResp must echo QID in SQID",
        );

        // Finish the (unidirectional) exchange, asserting the QID echo on
        // every remaining auth CapsuleResp so all five send sites — both
        // `ack_auth_send` and `send_auth_message` call sites — are covered.
        let (reply, _) = host_build_reply(&challenge, &secret, None);
        s.write_all(&build_auth_send_pdu(0x12, &reply))
            .await
            .unwrap();
        let reply_ack = read_pdu_async(&mut s).await;
        assert_eq!(reply_ack.header.pdu_type, pdu::PduType::CapsuleResp);
        assert_eq!(
            u16::from_le_bytes([reply_ack.body[10], reply_ack.body[11]]),
            1,
            "reply ACK must echo QID in SQID",
        );
        s.write_all(&build_auth_receive_pdu(0x13, 4096))
            .await
            .unwrap();
        let _success1 = read_pdu_async(&mut s).await; // C2HData (Success1)
        let succ_resp = read_pdu_async(&mut s).await;
        assert_eq!(succ_resp.header.pdu_type, pdu::PduType::CapsuleResp);
        assert_eq!(
            u16::from_le_bytes([succ_resp.body[10], succ_resp.body[11]]),
            1,
            "Success1 CapsuleResp must echo QID in SQID",
        );
        s.write_all(&build_auth_send_pdu(0x14, &dhwire::build_success2(0x55)))
            .await
            .unwrap();
        let final_ack = read_pdu_async(&mut s).await;
        assert_eq!(final_ack.header.pdu_type, pdu::PduType::CapsuleResp);
        assert_eq!(
            u16::from_le_bytes([final_ack.body[10], final_ack.body[11]]),
            1,
            "Success2 ACK must echo QID in SQID",
        );
        let _ = std::fs::remove_file(&path);
    }
}
