// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! iSCSI transport — connection lifecycle, PDU framing, login phase,
//! R2T loop. Lifted out of `thurvtld::iscsi::protocol` (Step 3c
//! phase 2) so both products share the same wire / framing surface
//! and only product-specific SCSI semantics live in their own crate.
//!
//! Surface:
//! - [`Pdu`] + [`read_pdu`] / [`write_pdu`] / [`build_empty_pdu`] —
//!   RFC 3720 §11 BHS framing (48-byte header + data segment +
//!   4-byte padding).
//! - [`handle_login_phase`] — RFC 3720 §7 login state machine
//!   (security / operational stages, CHAP via [`crate::auth`],
//!   parameter negotiation). Emits audit events through the
//!   [`LoginAuditSink`] trait so thurvtl can hook its audit channel
//!   in and thurvsa can no-op.
//! - [`collect_write_data`] — drains the unsolicited Data-Out burst
//!   and runs the R2T loop for the post-FirstBurst tail.
//! - [`serve_connection`] / [`run`] — accept loop and per-connection
//!   FFP loop, dispatching SCSI commands through
//!   [`crate::ScsiHandler::dispatch`].
//!
//! Audit / business-logic hooks (legal-hold sentinel readback, cloud
//! prefetch on READ, MOVE MEDIUM post-hooks, async SEND DIAGNOSTIC
//! self-test) stay inside the consuming product's `ScsiHandler` impl
//! — the transport calls `dispatch` and writes back the response, no
//! peeking at the CDB.

#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info};

use crate::auth::{ChapAlgorithm, ChapAuthenticator};
use crate::handler::{ScsiHandler, ScsiRequest, ScsiStatus};
use crate::session::{ADVERTISED_CMDSN_WINDOW, CmdSnVerdict, SessionManager};

// ===== Wire-level constants =====

/// Per-direction max bytes per Data-In / Data-Out PDU's data segment.
/// Cap that bounds [`MAX_RECV_DATA_SEGMENT_LENGTH`]'s pre-auth
/// allocation guard ([`read_pdu`]).
pub const MAX_RECV_DATA_SEGMENT_LENGTH: u32 = 131072;
/// Per-direction max bytes per solicited Data-Out burst (one R2T
/// sequence). Bounded by tape `READ BLOCK LIMITS`'s declared 16 MiB
/// max block, so a single host SCSI WRITE block fits in one R2T
/// cycle even though each individual Data-Out PDU is still capped at
/// `MAX_RECV_DATA_SEGMENT_LENGTH`.
pub const MAX_BURST_LENGTH: u32 = 16 * 1024 * 1024;
/// Max unsolicited bytes (immediate data + unsolicited Data-Out) per
/// SCSI Command. Held at `MAX_RECV_DATA_SEGMENT_LENGTH` so the full
/// unsolicited burst fits in the SCSI Command PDU's data segment with
/// no follow-on unsolicited Data-Out PDUs needed.
pub const FIRST_BURST_LENGTH: u32 = MAX_RECV_DATA_SEGMENT_LENGTH;

const TPGT: u16 = 1;

// Login Stages (RFC 3720 Section 5.3)
const STAGE_SECURITY: u8 = 0x00;
const STAGE_OPNEG: u8 = 0x01;
const STAGE_FULL: u8 = 0x03;

/// RFC 3720 negotiated session parameters carried through the login
/// state machine and echoed back in operational-parameter response
/// keys.
struct SessionParams {
    max_recv_data_segment_length: u32,
    immediate_data: bool,
    initial_r2t: bool,
    max_burst_length: u32,
    first_burst_length: u32,
    max_connections: u32,
    default_time2wait: u32,
    default_time2retain: u32,
}

impl Default for SessionParams {
    fn default() -> Self {
        Self {
            max_recv_data_segment_length: MAX_RECV_DATA_SEGMENT_LENGTH,
            immediate_data: true,
            initial_r2t: false,
            max_burst_length: MAX_BURST_LENGTH,
            first_burst_length: FIRST_BURST_LENGTH,
            max_connections: 1,
            default_time2wait: 2,
            default_time2retain: 0,
        }
    }
}

// ===== Audit hook =====

/// Login-phase event categories the consuming product can opt into
/// auditing. thurvtl implements this against its
/// `AuditChannel` + `AuditRateLimiter`; thurvsa passes
/// [`NoopLoginAudit`] (no audit subsystem yet).
pub enum LoginAuditEvent<'a> {
    ChapSuccess {
        peer: &'a str,
        initiator: Option<&'a str>,
        user: &'a str,
        algorithm: &'a str,
    },
    ChapFailure {
        peer: &'a str,
        initiator: Option<&'a str>,
        user: Option<&'a str>,
        reason: &'a str,
        error: String,
    },
}

/// Optional audit sink for login-phase events. The transport never
/// stores credentials — only the metadata fields above.
pub trait LoginAuditSink: Send + Sync {
    fn record(&self, event: LoginAuditEvent<'_>);
}

/// Factory that yields a fresh [`ChapAuthenticator`] on every iSCSI
/// login. The closure typically loads `<data_dir>/iscsi-users.json`
/// and runs [`ChapAuthenticator::from_file`] over it, capturing the
/// YAML-side `method` + `allowed_algorithms` policy parsed once at
/// startup. Parse-on-login is wire-safe: the authenticator is read
/// at exactly two sites inside `negotiate_chap_security_stage` and
/// never survives past the login phase, so credential file edits
/// take effect on the next session without restart or reload.
///
/// On factory error, [`handle_login_phase`] emits a
/// [`LoginAuditEvent::ChapFailure`] with `reason="config_load_failed"`
/// and rejects the login the same way it would reject an initiator
/// that skipped the security stage.
pub type ChapAuthFactory = Arc<dyn Fn() -> Result<ChapAuthenticator, anyhow::Error> + Send + Sync>;

/// Default no-op audit sink (thurvsa until it grows an audit
/// subsystem; tests).
#[derive(Default, Clone, Copy)]
pub struct NoopLoginAudit;

impl LoginAuditSink for NoopLoginAudit {
    fn record(&self, _event: LoginAuditEvent<'_>) {}
}

// ===== PDU =====

/// One iSCSI PDU as parsed off the wire. The 48-byte BHS plus the
/// already-padded data segment. Field accessors are the parsed bytes
/// — `bhs[..]` carries the raw header for handlers that need
/// opcode-specific fields (CDB, EDTL, R2T BufferOffset, …).
#[derive(Debug)]
pub struct Pdu {
    pub opcode: u8,
    pub immediate: bool,
    pub final_bit: bool,
    pub total_ahs_len: u8,
    pub data_segment_len: u32,
    pub lun: [u8; 8],
    pub itt: u32,
    pub ttt: u32,
    pub cmdsn: u32,
    pub expstatsn: u32,
    pub bhs: [u8; 48],
    pub data: Vec<u8>,
}

/// Allocate an empty BHS-only PDU for a target-issued response. The
/// caller fills in opcode-specific fields before [`write_pdu`].
pub fn build_empty_pdu(opcode: u8, immediate: bool, final_bit: bool) -> Pdu {
    Pdu {
        opcode,
        immediate,
        final_bit,
        total_ahs_len: 0,
        data_segment_len: 0,
        lun: [0; 8],
        itt: 0,
        ttt: 0,
        cmdsn: 0,
        expstatsn: 0,
        bhs: [0u8; 48],
        data: vec![],
    }
}

fn u24(b: &[u8]) -> u32 {
    ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32)
}

fn put_u24(dst: &mut [u8], v: u32) {
    dst[0] = (v >> 16) as u8;
    dst[1] = (v >> 8) as u8;
    dst[2] = v as u8;
}

fn pdu_expected_xfer_len(pdu: &Pdu) -> u32 {
    // RFC 3720 §10.3.1: EDTL is at SCSI Command BHS bytes 20..24.
    u32::from_be_bytes([pdu.bhs[20], pdu.bhs[21], pdu.bhs[22], pdu.bhs[23]])
}

fn opcode_name(op: u8) -> &'static str {
    match op & 0x3F {
        // Initiator opcodes
        0x00 => "NOP-Out",
        0x01 => "SCSI Command",
        0x02 => "Task Mgmt Req",
        0x03 => "Login Req",
        0x04 => "Text Req",
        0x05 => "Data-Out",
        0x06 => "Logout Req",
        // Target opcodes
        0x20 => "NOP-In",
        0x21 => "SCSI Resp",
        0x22 => "Task Mgmt Resp",
        0x23 => "Login Resp",
        0x24 => "Text Resp",
        0x25 => "Data-In",
        0x26 => "Logout Resp",
        0x3F => "Reject",
        _ => "Unknown",
    }
}

fn preview(bytes: &[u8], max: usize) -> String {
    use std::fmt::Write;
    let show = bytes.len().min(max);
    let mut s = String::new();
    for (i, b) in bytes[..show].iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(&mut s, "{:02x}", b);
    }
    if bytes.len() > show {
        s.push_str(" …");
    }
    s
}

fn format_text_data(data: &[u8]) -> String {
    let pairs: Vec<String> = data
        .split(|b| *b == 0u8)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect();
    pairs.join(", ")
}

fn parse_text_kv(buf: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for part in buf.split(|b| *b == 0u8) {
        if part.is_empty() {
            continue;
        }
        if let Some(eq) = part.iter().position(|b| *b == b'=') {
            let k = String::from_utf8_lossy(&part[..eq]).to_string();
            let v = String::from_utf8_lossy(&part[eq + 1..]).to_string();
            out.insert(k, v);
        }
    }
    out
}

fn push_kv(dst: &mut Vec<u8>, key: &str, val: &str) {
    dst.extend_from_slice(key.as_bytes());
    dst.push(b'=');
    dst.extend_from_slice(val.as_bytes());
    dst.push(0);
}

/// Read one BHS + data segment from `sock`. Returns `Err("graceful
/// disconnect")` on EOF / RST so the caller can swallow the
/// connection close without logging it as an error. Caps
/// `DataSegmentLength` at [`MAX_RECV_DATA_SEGMENT_LENGTH`] — an
/// attacker-controlled u24 would otherwise let one TCP connection
/// demand 16 MiB of pre-auth buffer.
pub async fn read_pdu<R: AsyncRead + Unpin>(sock: &mut R) -> Result<Pdu> {
    let mut bhs = [0u8; 48];
    if let Err(e) = sock.read_exact(&mut bhs).await {
        match e.kind() {
            io::ErrorKind::UnexpectedEof => {
                tracing::debug!("RX: peer closed connection (graceful disconnect)");
                return Err(anyhow!("graceful disconnect"));
            }
            io::ErrorKind::ConnectionReset => {
                tracing::debug!("RX: connection reset by peer");
                return Err(anyhow!("graceful disconnect"));
            }
            _ => {
                tracing::warn!("RX: error reading BHS: {}", e);
                return Err(anyhow!(e));
            }
        }
    }

    let opcode = bhs[0] & 0x3F;
    let immediate = (bhs[0] & 0x40) != 0;
    let final_bit = (bhs[1] & 0x80) != 0;
    let total_ahs_len = bhs[4];
    let data_segment_len = u24(&bhs[5..8]);
    let mut lun = [0u8; 8];
    lun.copy_from_slice(&bhs[8..16]);
    let itt = u32::from_be_bytes([bhs[16], bhs[17], bhs[18], bhs[19]]);
    let ttt = u32::from_be_bytes([bhs[20], bhs[21], bhs[22], bhs[23]]);
    let cmdsn = u32::from_be_bytes([bhs[24], bhs[25], bhs[26], bhs[27]]);
    let expstatsn = u32::from_be_bytes([bhs[28], bhs[29], bhs[30], bhs[31]]);

    if data_segment_len > MAX_RECV_DATA_SEGMENT_LENGTH {
        tracing::warn!(
            "RX: rejecting PDU with DataSegmentLength={} > MaxRecvDataSegmentLength={}",
            data_segment_len,
            MAX_RECV_DATA_SEGMENT_LENGTH
        );
        return Err(anyhow!(
            "DataSegmentLength {} exceeds MaxRecvDataSegmentLength {}",
            data_segment_len,
            MAX_RECV_DATA_SEGMENT_LENGTH
        ));
    }

    if total_ahs_len != 0 {
        let mut ahs = vec![0u8; (total_ahs_len as usize) * 4];
        if let Err(e) = sock.read_exact(&mut ahs).await {
            match e.kind() {
                io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset => {
                    return Err(anyhow!("graceful disconnect"));
                }
                _ => return Err(anyhow!(e)),
            }
        }
    }

    let mut data = vec![0u8; data_segment_len as usize];
    if data_segment_len > 0 {
        if let Err(e) = sock.read_exact(&mut data).await {
            match e.kind() {
                io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset => {
                    return Err(anyhow!("graceful disconnect"));
                }
                _ => return Err(anyhow!(e)),
            }
        }
        let pad = (4 - (data_segment_len % 4)) % 4;
        if pad > 0 {
            let mut tmp = vec![0u8; pad as usize];
            if let Err(e) = sock.read_exact(&mut tmp).await {
                match e.kind() {
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset => {
                        return Err(anyhow!("graceful disconnect"));
                    }
                    _ => return Err(anyhow!(e)),
                }
            }
        }
    }

    if tracing::enabled!(tracing::Level::DEBUG) {
        let csg = (bhs[1] >> 2) & 0x3;
        let nsg = bhs[1] & 0x3;
        tracing::debug!(
            "RX {:<12} op=0x{:02x} F={} I={} CSG={} NSG={} ITT=0x{:08x} CmdSN={} StatSN(exp)={} DataLen={} Data[{}]: {}",
            opcode_name(opcode),
            opcode,
            if final_bit { 1 } else { 0 },
            if immediate { 1 } else { 0 },
            csg,
            nsg,
            itt,
            cmdsn,
            expstatsn,
            data_segment_len,
            data.len().min(16),
            preview(&data, 16)
        );
    }

    Ok(Pdu {
        opcode,
        immediate,
        final_bit,
        total_ahs_len,
        data_segment_len,
        lun,
        itt,
        ttt,
        cmdsn,
        expstatsn,
        bhs,
        data,
    })
}

/// Serialize and transmit one PDU. Stamps opcode / immediate / final
/// bits + DataSegmentLength + ITT / TTT into the BHS; serials
/// (StatSN / ExpCmdSN / MaxCmdSN) are the caller's responsibility
/// (see [`stamp_serials_for_response`]).
pub async fn write_pdu<W: AsyncWrite + Unpin>(sock: &mut W, p: &mut Pdu) -> Result<()> {
    p.bhs[0] = (p.bhs[0] & 0xC0) | (p.opcode & 0x3F);
    if p.immediate {
        p.bhs[0] |= 0x40;
    }
    if p.final_bit {
        p.bhs[1] |= 0x80;
    }
    p.total_ahs_len = 0;
    p.data_segment_len = p.data.len() as u32;
    put_u24(&mut p.bhs[5..8], p.data_segment_len);
    p.bhs[16..20].copy_from_slice(&p.itt.to_be_bytes());
    p.bhs[20..24].copy_from_slice(&p.ttt.to_be_bytes());

    sock.write_all(&p.bhs).await?;
    if !p.data.is_empty() {
        sock.write_all(&p.data).await?;
        let pad = (4 - (p.data.len() as u32 % 4)) % 4;
        if pad > 0 {
            sock.write_all(&vec![0u8; pad as usize]).await?;
        }
    }
    sock.flush().await?;
    Ok(())
}

/// Compute the ExpCmdSN to put in a response PDU. RFC 3720/7143
/// §3.2.2.1 / §10.4: immediate PDUs (I-bit set) do not consume a
/// CmdSN, so a response to an immediate request must echo the
/// request's CmdSN unchanged. Non-immediate requests consume one.
pub fn next_exp_cmdsn(req: &Pdu) -> u32 {
    if req.immediate {
        req.cmdsn
    } else {
        req.cmdsn.wrapping_add(1)
    }
}

/// Stamp StatSN / ExpCmdSN / MaxCmdSN into a target-issued response
/// BHS at offsets 24 / 28 / 32.
pub fn stamp_serials_for_response(
    resp_bhs: &mut [u8; 48],
    statsn: u32,
    exp_cmdsn: u32,
    max_cmdsn: u32,
) {
    resp_bhs[24..28].copy_from_slice(&statsn.to_be_bytes());
    resp_bhs[28..32].copy_from_slice(&exp_cmdsn.to_be_bytes());
    resp_bhs[32..36].copy_from_slice(&max_cmdsn.to_be_bytes());
}

/// Synthesise a Target Transfer Tag for an R2T. Any value other than
/// 0xFFFFFFFF (reserved for unsolicited Data-Out) is legal; we mix
/// the command's ITT with the per-command R2TSN so a Data-Out reply
/// matching a stale TTT from a different command is detectable.
pub fn derive_r2t_ttt(itt: u32, r2tsn: u32) -> u32 {
    let ttt = 0x80000000u32 | itt.wrapping_add(r2tsn);
    if ttt == 0xFFFFFFFF { 0x80000000 } else { ttt }
}

/// Build the NOP-In PDU that answers a NOP-Out ping. RFC 7143 §11.19:
/// the Target Transfer Tag MUST carry the reserved value 0xFFFFFFFF
/// when the NOP-In is a *response* to a NOP-Out — a non-reserved TTT
/// is reserved for the target-initiated ping case. The Linux
/// initiator validates this strictly: a NOP-In whose ITT matches an
/// outstanding NOP-Out but whose TTT is not 0xFFFFFFFF is rejected
/// with ISCSI_ERR_PROTO, which tears the connection down. The ITT
/// echoes the NOP-Out's ITT. The caller stamps the serials
/// (StatSN / ExpCmdSN / MaxCmdSN) before [`write_pdu`].
pub fn build_nop_in_response(itt: u32) -> Pdu {
    let mut nop_in = build_empty_pdu(0x20, true, true);
    nop_in.itt = itt;
    nop_in.ttt = 0xFFFF_FFFF;
    nop_in
}

/// Send an R2T PDU (RFC 3720 §10.8) soliciting `ddtl` bytes starting
/// at `buffer_offset`. R2T does not advance StatSN — it carries the
/// StatSN that the next Status-bearing PDU will use.
async fn send_r2t<W: AsyncWrite + Unpin>(
    sock: &mut W,
    cmd: &Pdu,
    r2tsn: u32,
    ttt: u32,
    buffer_offset: u32,
    desired_data_transfer_length: u32,
    session_manager: &Arc<SessionManager>,
    tsih: u16,
    cid: u16,
) -> Result<()> {
    let mut r2t = build_empty_pdu(0x31, false, true);
    r2t.itt = cmd.itt;
    r2t.ttt = ttt;
    r2t.bhs[8..16].copy_from_slice(&cmd.lun);
    let statsn = session_manager.current_stat_sn(tsih, cid)?;
    let exp_cmdsn = next_exp_cmdsn(cmd);
    // The PDU reader runs concurrently with the R2T waiter (see the
    // demux in `serve_connection`), so the initiator may legitimately
    // pipeline new Cmd PDUs into the CmdSN window while we're parked
    // on Data-Out. Advertise the full window — `ADVERTISED_CMDSN_WINDOW`
    // is what gates how many in-flight Cmds the initiator may have.
    let max_cmdsn = exp_cmdsn.wrapping_add(ADVERTISED_CMDSN_WINDOW);
    stamp_serials_for_response(&mut r2t.bhs, statsn, exp_cmdsn, max_cmdsn);
    r2t.bhs[36..40].copy_from_slice(&r2tsn.to_be_bytes());
    r2t.bhs[40..44].copy_from_slice(&buffer_offset.to_be_bytes());
    r2t.bhs[44..48].copy_from_slice(&desired_data_transfer_length.to_be_bytes());
    write_pdu(sock, &mut r2t).await
}

/// Drain trailing Data-Out PDUs (and issue R2Ts for the
/// post-FirstBurst tail) of a host-to-target SCSI WRITE, appending
/// received bytes into `cmd.data` until `edtl` bytes are present.
/// See protocol.rs § "RFC 3720 §10.7-10.8" prose for the two-phase
/// design (unsolicited then R2T-solicited).
///
/// Data-Out PDUs are received over `data_out_rx`, which the PDU
/// reader task fills via the ITT-keyed route registered before
/// dispatch. R2T PDUs are written through `sock` (the connection's
/// write half). This decoupling lets the reader continue to demux
/// other in-flight Cmd / NopOut / Data-Out PDUs on the same TCP
/// stream concurrently with the R2T waiter — see `serve_connection`.
pub async fn collect_write_data<W: AsyncWrite + Unpin>(
    sock: &mut W,
    cmd: &mut Pdu,
    edtl: u32,
    session_manager: &Arc<SessionManager>,
    tsih: u16,
    cid: u16,
    data_out_rx: &mut tokio::sync::mpsc::Receiver<Pdu>,
) -> Result<()> {
    // Phase 1: drain unsolicited Data-Out, if any.
    if !cmd.final_bit && (cmd.data.len() as u32) < FIRST_BURST_LENGTH.min(edtl) {
        loop {
            let dout = data_out_rx.recv().await.ok_or_else(|| {
                anyhow!(
                    "PDU reader closed while awaiting unsolicited Data-Out for ITT=0x{:08x}",
                    cmd.itt
                )
            })?;
            // Reader pre-filters opcode 0x05 + matching ITT before
            // routing here, so the opcode/ITT checks below are
            // defense-in-depth.
            if (dout.opcode & 0x3F) != 0x05 {
                return Err(anyhow!(
                    "expected unsolicited Data-Out for ITT=0x{:08x}, got opcode 0x{:02x}",
                    cmd.itt,
                    dout.opcode
                ));
            }
            if dout.itt != cmd.itt {
                return Err(anyhow!(
                    "unsolicited Data-Out ITT mismatch: expected 0x{:08x}, got 0x{:08x}",
                    cmd.itt,
                    dout.itt
                ));
            }
            if dout.ttt != 0xFFFFFFFF {
                return Err(anyhow!(
                    "unsolicited Data-Out has non-default TTT=0x{:08x}",
                    dout.ttt
                ));
            }
            let buf_off =
                u32::from_be_bytes([dout.bhs[40], dout.bhs[41], dout.bhs[42], dout.bhs[43]]);
            if buf_off as usize != cmd.data.len() {
                return Err(anyhow!(
                    "unsolicited Data-Out BufferOffset {} != accumulated {}",
                    buf_off,
                    cmd.data.len()
                ));
            }
            let new_total = cmd.data.len() + dout.data.len();
            if new_total as u32 > edtl {
                return Err(anyhow!(
                    "unsolicited Data-Out overruns EDTL: {} + {} > {}",
                    cmd.data.len(),
                    dout.data.len(),
                    edtl
                ));
            }
            if new_total as u32 > FIRST_BURST_LENGTH {
                return Err(anyhow!(
                    "unsolicited burst {} exceeds FirstBurstLength {}",
                    new_total,
                    FIRST_BURST_LENGTH
                ));
            }
            cmd.data.extend_from_slice(&dout.data);
            if dout.final_bit {
                break;
            }
        }
    }

    // Phase 2: R2T-solicited bursts for the remainder.
    let mut r2tsn: u32 = 0;
    while (cmd.data.len() as u32) < edtl {
        let already = cmd.data.len() as u32;
        let remaining = edtl - already;
        let ddtl = MAX_BURST_LENGTH.min(remaining);
        let ttt = derive_r2t_ttt(cmd.itt, r2tsn);

        send_r2t(
            sock,
            cmd,
            r2tsn,
            ttt,
            already,
            ddtl,
            session_manager,
            tsih,
            cid,
        )
        .await?;

        let burst_end = already + ddtl;
        loop {
            let dout = data_out_rx.recv().await.ok_or_else(|| {
                anyhow!(
                    "PDU reader closed while awaiting solicited Data-Out for ITT=0x{:08x} TTT=0x{:08x}",
                    cmd.itt,
                    ttt
                )
            })?;
            if (dout.opcode & 0x3F) != 0x05 {
                return Err(anyhow!(
                    "expected solicited Data-Out for ITT=0x{:08x} TTT=0x{:08x}, got opcode 0x{:02x}",
                    cmd.itt,
                    ttt,
                    dout.opcode
                ));
            }
            if dout.itt != cmd.itt || dout.ttt != ttt {
                return Err(anyhow!(
                    "solicited Data-Out tag mismatch: ITT=0x{:08x} TTT=0x{:08x}",
                    dout.itt,
                    dout.ttt
                ));
            }
            let buf_off =
                u32::from_be_bytes([dout.bhs[40], dout.bhs[41], dout.bhs[42], dout.bhs[43]]);
            if buf_off != cmd.data.len() as u32 {
                return Err(anyhow!(
                    "solicited Data-Out BufferOffset {} != accumulated {}",
                    buf_off,
                    cmd.data.len()
                ));
            }
            let new_total = cmd.data.len() as u32 + dout.data.len() as u32;
            if new_total > burst_end {
                return Err(anyhow!(
                    "solicited Data-Out overruns R2T burst: {} + {} > {}",
                    cmd.data.len(),
                    dout.data.len(),
                    burst_end
                ));
            }
            cmd.data.extend_from_slice(&dout.data);
            let burst_done = (cmd.data.len() as u32) == burst_end;
            if dout.final_bit != burst_done {
                return Err(anyhow!(
                    "Data-Out F-bit / burst-end mismatch: F={} burst_done={}",
                    dout.final_bit,
                    burst_done
                ));
            }
            if burst_done {
                break;
            }
        }
        r2tsn = r2tsn.wrapping_add(1);
    }

    Ok(())
}

// ===== Login phase =====

async fn send_login_rejection(
    sock: &mut TcpStream,
    req: &Pdu,
    req_csg: u8,
    isid: &[u8; 6],
    statsn: &mut u32,
    status_class: u8,
    status_detail: u8,
) -> Result<()> {
    let mut resp = build_empty_pdu(0x23, false, true);
    resp.itt = req.itt;
    resp.bhs[1] = ((req_csg & 0x3) << 2) | (req_csg & 0x3);
    resp.bhs[2] = 0x00;
    resp.bhs[3] = 0x00;
    resp.bhs[8..14].copy_from_slice(isid);
    resp.bhs[14..16].copy_from_slice(&0x0000u16.to_be_bytes());
    resp.bhs[36] = status_class;
    resp.bhs[37] = status_detail;
    resp.data = Vec::new();

    let exp_cmdsn = req.cmdsn;
    let max_cmdsn = exp_cmdsn.wrapping_add(ADVERTISED_CMDSN_WINDOW);
    stamp_serials_for_response(&mut resp.bhs, *statsn, exp_cmdsn, max_cmdsn);
    *statsn = statsn.wrapping_add(1);

    write_pdu(sock, &mut resp).await
}

/// Outcome of the login phase, threaded back into [`serve_connection`]
/// so the FFP loop knows the session bindings.
pub struct LoginOutcome {
    pub is_discovery: bool,
    pub tsih: u16,
    pub cid: u16,
    pub statsn: u32,
    pub initiator_iqn: Option<String>,
    pub authenticated_partition: Option<String>,
}

/// CHAP state carried across login PDUs. Lives inside the login state
/// machine; not exposed to consumers.
struct ChapState {
    challenge: Option<Vec<u8>>,
    identifier: u8,
    algorithm: Option<ChapAlgorithm>,
    waiting_for_response: bool,
    authenticated_user: Option<String>,
    authenticated_partition: Option<String>,
}

impl ChapState {
    fn new() -> Self {
        Self {
            challenge: None,
            identifier: 1,
            algorithm: None,
            waiting_for_response: false,
            authenticated_user: None,
            authenticated_partition: None,
        }
    }
}

/// Run one round of the CHAP security stage. Mutates `state` as
/// negotiation progresses, appends response keys, and returns
/// `Ok(transit_allowed)` — `false` while CHAP is mid-flight so the
/// outer login loop keeps the T-bit clear.
fn negotiate_chap_security_stage(
    state: &mut ChapState,
    authenticator: &ChapAuthenticator,
    req_keys: &HashMap<String, String>,
    initiator_name: Option<&str>,
    audit: &dyn LoginAuditSink,
    peer: &str,
    resp_keys: &mut Vec<u8>,
) -> Result<bool> {
    if let Some(auth_method) = req_keys.get("AuthMethod")
        && !auth_method.contains("CHAP")
    {
        return Err(anyhow!("CHAP authentication required"));
    }

    if !state.waiting_for_response {
        let Some(chap_a) = req_keys.get("CHAP_A") else {
            push_kv(resp_keys, "AuthMethod", "CHAP");
            return Ok(false);
        };
        let selected = crate::auth::select_algorithm(chap_a, authenticator.allowed_algorithms())
            .ok_or_else(|| anyhow!("No common CHAP algorithm (offered: {})", chap_a))?;

        let challenge = authenticator.generate_challenge();
        state.identifier = (state.identifier % 255) + 1;

        push_kv(resp_keys, "CHAP_A", &selected.id().to_string());
        push_kv(resp_keys, "CHAP_I", &state.identifier.to_string());
        push_kv(
            resp_keys,
            "CHAP_C",
            &format!("0x{}", hex::encode(&challenge)),
        );

        state.challenge = Some(challenge);
        state.algorithm = Some(selected);
        state.waiting_for_response = true;
        return Ok(false);
    }

    let username = req_keys
        .get("CHAP_N")
        .ok_or_else(|| anyhow!("Missing CHAP_N"))?;
    let response_hex = req_keys
        .get("CHAP_R")
        .ok_or_else(|| anyhow!("Missing CHAP_R"))?;
    let response_hex = response_hex.trim_start_matches("0x");
    let response = hex::decode(response_hex).map_err(|e| anyhow!("Invalid CHAP_R hex: {}", e))?;
    let challenge = state
        .challenge
        .as_ref()
        .ok_or_else(|| anyhow!("No CHAP challenge sent"))?;
    let algorithm = state
        .algorithm
        .ok_or_else(|| anyhow!("No CHAP algorithm negotiated"))?;

    match authenticator.verify_response(username, challenge, state.identifier, &response, algorithm)
    {
        Ok(true) => {}
        Ok(false) => {
            audit.record(LoginAuditEvent::ChapFailure {
                peer,
                initiator: initiator_name,
                user: Some(username),
                reason: "invalid_response",
                error: "invalid response".into(),
            });
            return Err(anyhow!("CHAP authentication failed"));
        }
        Err(e) => {
            audit.record(LoginAuditEvent::ChapFailure {
                peer,
                initiator: initiator_name,
                user: Some(username),
                reason: "verify_error",
                error: e.to_string(),
            });
            return Err(anyhow!("CHAP authentication error: {}", e));
        }
    }

    state.authenticated_user = Some(username.clone());
    state.authenticated_partition = authenticator
        .get_user(username)
        .and_then(|u| u.partition().map(String::from));
    audit.record(LoginAuditEvent::ChapSuccess {
        peer,
        initiator: initiator_name,
        user: username,
        algorithm: algorithm.name(),
    });

    if authenticator.requires_mutual_chap(username) {
        let init_challenge_hex = req_keys
            .get("CHAP_C")
            .ok_or_else(|| anyhow!("Missing CHAP_C for mutual CHAP"))?;
        let init_challenge_hex = init_challenge_hex.trim_start_matches("0x");
        let init_challenge =
            hex::decode(init_challenge_hex).map_err(|e| anyhow!("Invalid CHAP_C hex: {}", e))?;
        let init_identifier = req_keys
            .get("CHAP_I")
            .and_then(|s| s.parse::<u8>().ok())
            .ok_or_else(|| anyhow!("Missing CHAP_I for mutual CHAP"))?;

        let target_response =
            authenticator.compute_target_response(&init_challenge, init_identifier, algorithm)?;

        let target_name = authenticator
            .get_target_username()
            .ok_or_else(|| anyhow!("Target username unconfigured"))?;

        push_kv(resp_keys, "CHAP_N", target_name);
        push_kv(
            resp_keys,
            "CHAP_R",
            &format!("0x{}", hex::encode(&target_response)),
        );
    }

    Ok(true)
}

/// Build the operational-parameter response keys for one login PDU.
/// Mirrors what the initiator offered (digests, markers, time2*),
/// pinning each at the safest target-side value.
fn append_opneg_response_keys(
    req_keys: &HashMap<String, String>,
    params: &SessionParams,
    is_discovery: bool,
    resp_keys: &mut Vec<u8>,
) {
    if !is_discovery {
        push_kv(resp_keys, "TargetPortalGroupTag", &TPGT.to_string());
    }
    if let Some(erl) = req_keys.get("ErrorRecoveryLevel") {
        push_kv(resp_keys, "ErrorRecoveryLevel", erl);
    }
    if req_keys.contains_key("HeaderDigest") {
        push_kv(resp_keys, "HeaderDigest", "None");
    }
    if req_keys.contains_key("DataDigest") {
        push_kv(resp_keys, "DataDigest", "None");
    }
    if req_keys.contains_key("OFMarker") {
        push_kv(resp_keys, "OFMarker", "No");
    }
    if req_keys.contains_key("IFMarker") {
        push_kv(resp_keys, "IFMarker", "No");
    }
    push_kv(
        resp_keys,
        "MaxRecvDataSegmentLength",
        &params.max_recv_data_segment_length.to_string(),
    );
    if let Some(t2w) = req_keys.get("DefaultTime2Wait") {
        push_kv(resp_keys, "DefaultTime2Wait", t2w);
    } else {
        push_kv(
            resp_keys,
            "DefaultTime2Wait",
            &params.default_time2wait.to_string(),
        );
    }
    if let Some(t2r) = req_keys.get("DefaultTime2Retain") {
        push_kv(resp_keys, "DefaultTime2Retain", t2r);
    } else {
        push_kv(
            resp_keys,
            "DefaultTime2Retain",
            &params.default_time2retain.to_string(),
        );
    }
    if !is_discovery {
        push_kv(
            resp_keys,
            "ImmediateData",
            if params.immediate_data { "Yes" } else { "No" },
        );
        push_kv(
            resp_keys,
            "InitialR2T",
            if params.initial_r2t { "Yes" } else { "No" },
        );
        push_kv(
            resp_keys,
            "MaxBurstLength",
            &params.max_burst_length.to_string(),
        );
        push_kv(
            resp_keys,
            "FirstBurstLength",
            &params.first_burst_length.to_string(),
        );
        push_kv(
            resp_keys,
            "MaxConnections",
            &params.max_connections.to_string(),
        );
    }
}

/// RFC 3720-compliant login phase: security stage (CHAP if enabled),
/// operational-parameter negotiation, and final transition into Full
/// Feature Phase. Returns the bound TSIH / CID + the captured
/// initiator IQN + the partition the CHAP user maps to (if any).
pub async fn handle_login_phase(
    sock: &mut TcpStream,
    target_iqn: &str,
    session_manager: Arc<SessionManager>,
    auth: Option<&ChapAuthFactory>,
    audit: &dyn LoginAuditSink,
    peer: &str,
) -> Result<LoginOutcome> {
    let mut current_stage = STAGE_SECURITY;
    let params = SessionParams::default();
    let mut isid = [0u8; 6];
    let mut is_discovery = false;
    let mut tsih: u16 = 0;
    let cid: u16 = 0;
    let mut statsn: u32 = 1;

    let mut chap = ChapState::new();
    let mut initiator_name: Option<String> = None;

    // Materialize the authenticator once at the top of the login.
    // The factory closure loads iscsi-users.json fresh on every
    // call, so file edits between sessions are picked up here
    // without any reload primitive. Parse failure rejects the login
    // with the same `0x02/0x01` ("authentication failure") status
    // the security-stage-skipped path uses.
    let chap_auth: Option<ChapAuthenticator> = match auth {
        Some(factory) => match factory() {
            Ok(a) => Some(a),
            Err(e) => {
                audit.record(LoginAuditEvent::ChapFailure {
                    peer,
                    initiator: initiator_name.as_deref(),
                    user: None,
                    reason: "config_load_failed",
                    error: e.to_string(),
                });
                // Read one Login PDU so we can build a coherent
                // rejection PDU (NSG / CSG / ISID echoed back). If
                // the read fails the connection's already broken.
                let req = read_pdu(sock).await?;
                let req_csg = (req.bhs[1] >> 2) & 0x3;
                isid.copy_from_slice(&req.bhs[8..14]);
                send_login_rejection(sock, &req, req_csg, &isid, &mut statsn, 0x02, 0x01).await?;
                return Err(anyhow!("CHAP config load failed: {}", e));
            }
        },
        None => None,
    };

    loop {
        let req = read_pdu(sock).await?;
        if (req.opcode & 0x3F) != 0x03 {
            return Err(anyhow!(
                "expected Login Request, got opcode 0x{:02x}",
                req.opcode
            ));
        }

        let req_csg = (req.bhs[1] >> 2) & 0x3;
        let req_nsg = req.bhs[1] & 0x3;
        let req_transit = (req.bhs[1] & 0x80) != 0;

        let req_keys = parse_text_kv(&req.data);

        if initiator_name.is_none()
            && let Some(name) = req_keys.get("InitiatorName")
        {
            initiator_name = Some(name.clone());
        }

        if current_stage == STAGE_SECURITY {
            is_discovery = req_keys
                .get("SessionType")
                .map(|v| v.eq_ignore_ascii_case("Discovery"))
                .unwrap_or(false);
            isid.copy_from_slice(&req.bhs[8..14]);
        }

        let mut resp_keys = Vec::new();
        let mut transit = req_transit;
        let negotiate_opneg =
            req_csg == STAGE_OPNEG || (req_csg == STAGE_SECURITY && req_nsg == STAGE_FULL);

        match req_csg {
            STAGE_SECURITY => {
                if let Some(ref authenticator) = chap_auth {
                    let allow_transit = negotiate_chap_security_stage(
                        &mut chap,
                        authenticator,
                        &req_keys,
                        initiator_name.as_deref(),
                        audit,
                        peer,
                        &mut resp_keys,
                    )?;
                    if !allow_transit {
                        transit = false;
                    }
                } else {
                    push_kv(&mut resp_keys, "AuthMethod", "None");
                    if !is_discovery {
                        push_kv(&mut resp_keys, "TargetName", target_iqn);
                    }
                }
            }
            STAGE_OPNEG => {
                if chap_auth.is_some()
                    && !chap.waiting_for_response
                    && chap.authenticated_user.is_none()
                {
                    audit.record(LoginAuditEvent::ChapFailure {
                        peer,
                        initiator: initiator_name.as_deref(),
                        user: None,
                        reason: "skipped_security_stage",
                        error: "auth required, security stage skipped".into(),
                    });
                    send_login_rejection(sock, &req, req_csg, &isid, &mut statsn, 0x02, 0x01)
                        .await?;
                    return Err(anyhow!("Authentication required - rejected initiator"));
                }
            }
            _ => {}
        }

        if negotiate_opneg {
            append_opneg_response_keys(&req_keys, &params, is_discovery, &mut resp_keys);
        }

        let resp_csg = req_csg;
        let resp_nsg = if transit { req_nsg } else { req_csg };
        let entering_ffp = transit && req_nsg == STAGE_FULL;

        let mut resp = build_empty_pdu(0x23, false, false);
        resp.itt = req.itt;
        resp.bhs[2] = 0x00;
        resp.bhs[3] = 0x00;
        resp.bhs[8..14].copy_from_slice(&isid);

        if entering_ffp {
            if is_discovery {
                tsih = 0x0000;
            } else {
                tsih = session_manager.create_session(isid);
                session_manager
                    .add_connection(tsih, cid, params.max_recv_data_segment_length)
                    .map_err(|e| anyhow!("Failed to add connection: {}", e))?;
            }
            resp.bhs[14..16].copy_from_slice(&tsih.to_be_bytes());
            resp.bhs[1] = 0x80 | ((resp_csg & 0x3) << 2) | (resp_nsg & 0x3);
        } else {
            resp.bhs[14..16].copy_from_slice(&0x0000u16.to_be_bytes());
            if transit {
                resp.bhs[1] = 0x80 | ((resp_csg & 0x3) << 2) | (resp_nsg & 0x3);
            } else {
                resp.bhs[1] = ((resp_csg & 0x3) << 2) | (resp_nsg & 0x3);
            }
        }
        resp.bhs[36] = 0;
        resp.bhs[37] = 0;

        let exp_cmdsn = req.cmdsn;
        let max_cmdsn = exp_cmdsn.wrapping_add(ADVERTISED_CMDSN_WINDOW);
        resp.data = resp_keys;

        let current_statsn = statsn;
        statsn = statsn.wrapping_add(1);
        stamp_serials_for_response(&mut resp.bhs, current_statsn, exp_cmdsn, max_cmdsn);
        write_pdu(sock, &mut resp).await?;

        if entering_ffp {
            if !is_discovery
                && tsih != 0
                && let Err(e) =
                    session_manager.set_partition(tsih, chap.authenticated_partition.clone())
            {
                tracing::warn!(
                    "Failed to bind partition {:?} to TSIH={}: {}",
                    chap.authenticated_partition,
                    tsih,
                    e
                );
            }
            return Ok(LoginOutcome {
                is_discovery,
                tsih,
                cid,
                statsn,
                initiator_iqn: initiator_name,
                authenticated_partition: chap.authenticated_partition,
            });
        }

        if transit {
            current_stage = resp_nsg;
            let _ = current_stage;
        }
    }
}

// ===== Per-connection FFP loop =====

/// RAII guard: invokes `handler.on_session_close()` and removes the
/// connection from the session manager when the connection drops.
struct SessionGuard<H: ScsiHandler + ?Sized> {
    session_manager: Arc<SessionManager>,
    handler: Arc<H>,
    tsih: u16,
    cid: u16,
}

impl<H: ScsiHandler + ?Sized> Drop for SessionGuard<H> {
    fn drop(&mut self) {
        info!("Cleaning up session TSIH={} CID={}", self.tsih, self.cid);
        self.handler.on_session_close(self.tsih, self.cid);
        let _ = self.session_manager.remove_connection(self.tsih, self.cid);
    }
}

/// Drive one iSCSI connection from accept to drop. Runs the login
/// phase, then the FFP read-PDU loop, dispatching SCSI commands
/// through `handler.dispatch()`.
/// Routed PDU emitted by the per-connection reader task. `Some(rx)`
/// is only set on SCSI Cmd PDUs with the W-bit (write) — the reader
/// pre-registers the Data-Out route under the cmd's ITT and ships the
/// Receiver back to main so the dispatch loop can hand it to
/// `collect_write_data`. Every other classified PDU carries `None`.
struct RoutedPdu {
    pdu: Pdu,
    data_out_rx: Option<tokio::sync::mpsc::Receiver<Pdu>>,
}

/// Bounded depth of the per-ITT Data-Out channel. One unsolicited
/// burst is bounded by `FIRST_BURST_LENGTH` and one R2T-solicited
/// burst by `MAX_BURST_LENGTH`; at the 128 KiB segment cap, the
/// largest legal burst is ~128 PDUs (16 MiB / 128 KiB). 32 buffered
/// is enough headroom that the reader rarely backpressures; the
/// reader awaits on `send().await` when full so back-pressure
/// naturally throttles the wire.
const DATA_OUT_CHANNEL_DEPTH: usize = 32;

/// Per-connection PDU reader. Owns the read half of the TCP stream
/// and runs as a spawned task. Demuxes inbound PDUs by ITT:
///
/// - **SCSI Cmd (op 0x01) with W-bit set** — pre-register a
///   bounded mpsc channel under the cmd's ITT in the shared route
///   map, ship `(cmd_pdu, Some(receiver))` to main. The race window
///   where unsolicited Data-Out could arrive before main has a
///   chance to register is closed here.
/// - **Data-Out (op 0x05)** — look up the route by ITT. Match →
///   forward to the per-ITT channel (back-pressured if full). No
///   match → forward to main as a stray PDU; main emits Reject.
/// - **Everything else** — forward to main as `(pdu, None)`.
///
/// Exits on read error (forwards the `Err` to main, which then
/// returns) or when main drops the receiving end of `main_tx`.
async fn pdu_reader(
    mut read_half: tokio::net::tcp::OwnedReadHalf,
    main_tx: tokio::sync::mpsc::Sender<Result<RoutedPdu>>,
    routes: Arc<std::sync::Mutex<HashMap<u32, tokio::sync::mpsc::Sender<Pdu>>>>,
) {
    loop {
        let pdu = match read_pdu(&mut read_half).await {
            Ok(p) => p,
            Err(e) => {
                let _ = main_tx.send(Err(e)).await;
                break;
            }
        };
        let op = pdu.opcode & 0x3F;
        if op == 0x01 && (pdu.bhs[1] & 0x20) != 0 {
            // SCSI Cmd with W-bit: pre-register the Data-Out route
            // before forwarding the Cmd to main. Closes the race
            // where the next PDU on the wire could be the start of
            // this Cmd's unsolicited Data-Out burst.
            let (tx, rx) = tokio::sync::mpsc::channel(DATA_OUT_CHANNEL_DEPTH);
            {
                let mut routes = routes.lock().expect("route map mutex poisoned");
                routes.insert(pdu.itt, tx);
            }
            if main_tx
                .send(Ok(RoutedPdu {
                    pdu,
                    data_out_rx: Some(rx),
                }))
                .await
                .is_err()
            {
                break;
            }
        } else if op == 0x05 {
            // Data-Out — route by ITT. If no route registered, fall
            // through to main so the existing stray-Data-Out path
            // fires a Reject.
            let route = {
                let routes = routes.lock().expect("route map mutex poisoned");
                routes.get(&pdu.itt).cloned()
            };
            if let Some(tx) = route {
                if tx.send(pdu).await.is_err() {
                    // collect_write_data hung up or completed —
                    // silently drop late Data-Outs.
                }
            } else if main_tx
                .send(Ok(RoutedPdu {
                    pdu,
                    data_out_rx: None,
                }))
                .await
                .is_err()
            {
                break;
            }
        } else if main_tx
            .send(Ok(RoutedPdu {
                pdu,
                data_out_rx: None,
            }))
            .await
            .is_err()
        {
            break;
        }
    }
}

pub async fn serve_connection<H: ScsiHandler + ?Sized>(
    mut sock: TcpStream,
    handler: Arc<H>,
    session_manager: Arc<SessionManager>,
    auth: Option<&ChapAuthFactory>,
    audit: &dyn LoginAuditSink,
    peer: &str,
) -> Result<()> {
    let target_iqn = handler.target_iqn();
    let outcome = handle_login_phase(
        &mut sock,
        target_iqn,
        Arc::clone(&session_manager),
        auth,
        audit,
        peer,
    )
    .await?;
    let LoginOutcome {
        tsih,
        cid,
        mut statsn,
        initiator_iqn,
        authenticated_partition,
        ..
    } = outcome;

    let _guard: SessionGuard<H> = SessionGuard {
        session_manager: Arc::clone(&session_manager),
        handler: Arc::clone(&handler),
        tsih,
        cid,
    };

    // Split the socket so the PDU reader task can drain the read half
    // concurrently with the FFP dispatch loop's writes + R2T waits.
    let (read_half, mut write_half) = sock.into_split();
    let routes: Arc<std::sync::Mutex<HashMap<u32, tokio::sync::mpsc::Sender<Pdu>>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));
    let (main_tx, mut main_rx) =
        tokio::sync::mpsc::channel::<Result<RoutedPdu>>(DATA_OUT_CHANNEL_DEPTH);
    let reader_task = tokio::spawn(pdu_reader(read_half, main_tx, Arc::clone(&routes)));
    // Abort the reader task on any exit path so the OwnedReadHalf is
    // dropped promptly (Drop closes the TCP read side).
    struct AbortOnDrop(tokio::task::JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _reader_guard = AbortOnDrop(reader_task);

    loop {
        let routed = match main_rx.recv().await {
            Some(Ok(r)) => r,
            Some(Err(e)) => return Err(e),
            None => return Err(anyhow!("PDU reader exited without forwarding error")),
        };
        let mut pdu = routed.pdu;
        let mut data_out_rx = routed.data_out_rx;

        let _ = session_manager.update_activity(tsih);

        if tsih != 0 {
            match session_manager.check_cmdsn(tsih, cid, pdu.cmdsn, pdu.immediate) {
                Ok(CmdSnVerdict::Accept) | Ok(CmdSnVerdict::Duplicate) => {}
                Ok(CmdSnVerdict::OutOfWindow) => {
                    return Err(anyhow!(
                        "out-of-window CmdSN {} on TSIH={} CID={}",
                        pdu.cmdsn,
                        tsih,
                        cid
                    ));
                }
                Err(e) => {
                    return Err(anyhow!("CmdSN check failed: {}", e));
                }
            }
        }

        // Shadow the old `sock` binding: every write below targets
        // the OwnedWriteHalf the reader task does not touch.
        let sock = &mut write_half;

        // Helper: drop the Data-Out route once a WRITE-bearing Cmd
        // finishes (success or error). Safe to call even when the
        // ITT was never registered (no-op).
        let drop_route = |itt: u32| {
            let mut routes = routes.lock().expect("route map mutex poisoned");
            routes.remove(&itt);
        };

        match pdu.opcode & 0x3F {
            0x00 => {
                // NOP-Out ping; answer with a NOP-In.
                let mut nop_in = build_nop_in_response(pdu.itt);
                let exp_cmdsn = next_exp_cmdsn(&pdu);
                let max_cmdsn = exp_cmdsn.wrapping_add(ADVERTISED_CMDSN_WINDOW);
                let sn = session_manager.get_and_increment_statsn(tsih, cid)?;
                stamp_serials_for_response(&mut nop_in.bhs, sn, exp_cmdsn, max_cmdsn);
                write_pdu(sock, &mut nop_in).await?;
            }
            0x04 => {
                // Text Request — SendTargets discovery
                let req_keys = parse_text_kv(&pdu.data);
                let mut tx = build_empty_pdu(0x24, false, true);
                tx.itt = pdu.itt;
                let exp_cmdsn = next_exp_cmdsn(&pdu);
                let max_cmdsn = exp_cmdsn.wrapping_add(ADVERTISED_CMDSN_WINDOW);
                let sn = if tsih == 0 {
                    let s = statsn;
                    statsn = statsn.wrapping_add(1);
                    s
                } else {
                    session_manager.get_and_increment_statsn(tsih, cid)?
                };
                stamp_serials_for_response(&mut tx.bhs, sn, exp_cmdsn, max_cmdsn);

                if let Some(st) = req_keys.get("SendTargets") {
                    let addr_str = sock
                        .local_addr()
                        .map(|a| format!("{}:{}", a.ip(), a.port()))
                        .unwrap_or_else(|_| "127.0.0.1:3260".to_string());
                    let mut lines = Vec::<u8>::new();
                    if st.eq_ignore_ascii_case("All") || st == target_iqn {
                        push_kv(&mut lines, "TargetName", target_iqn);
                        push_kv(
                            &mut lines,
                            "TargetAddress",
                            &format!("{},{}", addr_str, TPGT),
                        );
                    }
                    tx.data = lines;
                } else {
                    let keys = format!(
                        "TargetPortalGroupTag={}\0\
                         MaxRecvDataSegmentLength={}\0\
                         ImmediateData=Yes\0\
                         InitialR2T=No\0\
                         MaxBurstLength={}\0\
                         FirstBurstLength={}\0",
                        TPGT, MAX_RECV_DATA_SEGMENT_LENGTH, MAX_BURST_LENGTH, FIRST_BURST_LENGTH
                    )
                    .into_bytes();
                    tx.data = keys;
                }
                let _ = format_text_data(&tx.data);
                write_pdu(sock, &mut tx).await?;
            }
            0x01 => {
                // SCSI Command. Drain Data-Out (R2T loop) before
                // dispatch — the handler sees a complete payload.
                // The reader pre-registered an ITT-keyed Data-Out
                // route iff the W-bit was set; drop it after the
                // burst completes (success or error).
                let edtl = pdu_expected_xfer_len(&pdu);
                let w_bit = (pdu.bhs[1] & 0x20) != 0;
                if w_bit && edtl as usize > pdu.data.len() {
                    let rx = data_out_rx.as_mut().ok_or_else(|| {
                        anyhow!(
                            "reader did not register Data-Out route for ITT=0x{:08x}",
                            pdu.itt
                        )
                    })?;
                    let outcome =
                        collect_write_data(sock, &mut pdu, edtl, &session_manager, tsih, cid, rx)
                            .await;
                    drop_route(pdu.itt);
                    outcome?;
                } else if w_bit {
                    // W-bit set but unsolicited data already covers EDTL —
                    // still drop the registered route to release the
                    // bounded channel slot.
                    drop_route(pdu.itt);
                }
                // data_out_rx drops at end of loop iteration; reader's
                // subsequent send() on the closed channel is silently
                // discarded (which is what we want — any straggling
                // Data-Out PDUs after burst completion are spec-illegal).

                let lun = u64::from(pdu.lun[1]);
                let cdb_slice: [u8; 16] = {
                    let mut c = [0u8; 16];
                    c.copy_from_slice(&pdu.bhs[32..48]);
                    c
                };

                let req = ScsiRequest {
                    tsih,
                    cid,
                    lun,
                    cdb: &cdb_slice,
                    data_out: &pdu.data,
                    data_in_max: edtl as usize,
                    initiator_iqn: initiator_iqn.as_deref(),
                    peer,
                    session_partition: authenticated_partition.as_deref(),
                };

                let resp = handler.dispatch(req).await;

                let exp_cmdsn = next_exp_cmdsn(&pdu);
                let max_cmdsn = exp_cmdsn.wrapping_add(ADVERTISED_CMDSN_WINDOW);

                if !resp.data_in.is_empty() {
                    // Truncate to initiator-advertised EDTL.
                    let mut data_in = resp.data_in;
                    if data_in.len() as u32 > edtl {
                        data_in.truncate(edtl as usize);
                    }
                    let actual_len = data_in.len() as u32;
                    let residual = edtl.saturating_sub(actual_len);

                    let mut din = build_empty_pdu(0x25, true, true);
                    din.itt = pdu.itt;
                    din.ttt = 0xFFFFFFFF;
                    din.bhs[1] = 0x80 | 0x01; // F | S
                    if residual > 0 {
                        din.bhs[1] |= 0x02; // U (residual underflow)
                    }
                    din.bhs[3] = resp.status.code();
                    din.bhs[44..48].copy_from_slice(&residual.to_be_bytes());

                    let sn = session_manager.get_and_increment_statsn(tsih, cid)?;
                    stamp_serials_for_response(&mut din.bhs, sn, exp_cmdsn, max_cmdsn);
                    din.data = data_in;
                    write_pdu(sock, &mut din).await?;
                } else {
                    // SCSI Response only.
                    let mut sresp = build_empty_pdu(0x21, true, true);
                    sresp.itt = pdu.itt;
                    sresp.bhs[3] = resp.status.code();
                    if matches!(resp.status, ScsiStatus::CheckCondition) {
                        let sense = match resp.sense {
                            Some(s) => s.to_bytes(),
                            None => {
                                // Default sense (ILLEGAL REQUEST /
                                // INVALID COMMAND OPERATION CODE) —
                                // should never happen because handlers
                                // fill it in, but we'd rather emit a
                                // usable wire response than an empty
                                // data segment.
                                vec![
                                    0x70, 0, 0x05, 0, 0, 0, 0, 10, 0, 0, 0, 0, 0x20, 0, 0, 0, 0, 0,
                                ]
                            }
                        };
                        let len = sense.len() as u16;
                        let mut payload = Vec::with_capacity(2 + sense.len());
                        payload.extend_from_slice(&len.to_be_bytes());
                        payload.extend_from_slice(&sense);
                        sresp.data = payload;
                    }
                    let sn = session_manager.get_and_increment_statsn(tsih, cid)?;
                    stamp_serials_for_response(&mut sresp.bhs, sn, exp_cmdsn, max_cmdsn);
                    write_pdu(sock, &mut sresp).await?;
                }
            }
            0x02 => {
                // Task Management — function complete.
                let mut tmf = build_empty_pdu(0x22, true, true);
                tmf.itt = pdu.itt;
                tmf.bhs[2] = 0x00;
                let exp_cmdsn = next_exp_cmdsn(&pdu);
                let max_cmdsn = exp_cmdsn.wrapping_add(ADVERTISED_CMDSN_WINDOW);
                let sn = session_manager.get_and_increment_statsn(tsih, cid)?;
                stamp_serials_for_response(&mut tmf.bhs, sn, exp_cmdsn, max_cmdsn);
                write_pdu(sock, &mut tmf).await?;
            }
            0x06 => {
                // Logout — connection closed successfully.
                let mut logout_resp = build_empty_pdu(0x26, true, true);
                logout_resp.itt = pdu.itt;
                logout_resp.bhs[2] = 0x00;
                let exp_cmdsn = next_exp_cmdsn(&pdu);
                let max_cmdsn = exp_cmdsn.wrapping_add(ADVERTISED_CMDSN_WINDOW);
                let sn = session_manager.get_and_increment_statsn(tsih, cid)?;
                stamp_serials_for_response(&mut logout_resp.bhs, sn, exp_cmdsn, max_cmdsn);
                write_pdu(sock, &mut logout_resp).await?;
                break;
            }
            0x05 => {
                // Stray Data-Out at the FFP top — protocol error.
                tracing::warn!(
                    "Stray Data-Out PDU ITT=0x{:08x} TTT=0x{:08x}",
                    pdu.itt,
                    pdu.ttt
                );
                let mut rej = build_empty_pdu(0x3F, true, true);
                rej.itt = 0xFFFFFFFF;
                rej.bhs[2] = 0x04; // Protocol error
                let exp_cmdsn = next_exp_cmdsn(&pdu);
                let max_cmdsn = exp_cmdsn.wrapping_add(ADVERTISED_CMDSN_WINDOW);
                let sn = session_manager.get_and_increment_statsn(tsih, cid)?;
                stamp_serials_for_response(&mut rej.bhs, sn, exp_cmdsn, max_cmdsn);
                write_pdu(sock, &mut rej).await?;
            }
            0xFF => break,
            _ => {
                error!(
                    "Unsupported opcode: 0x{:02x} ({})",
                    pdu.opcode & 0x3F,
                    opcode_name(pdu.opcode)
                );
                let mut rej = build_empty_pdu(0x3F, true, true);
                rej.itt = pdu.itt;
                rej.bhs[2] = 0x09; // Command not supported
                let exp_cmdsn = next_exp_cmdsn(&pdu);
                let max_cmdsn = exp_cmdsn.wrapping_add(ADVERTISED_CMDSN_WINDOW);
                let sn = session_manager.get_and_increment_statsn(tsih, cid)?;
                stamp_serials_for_response(&mut rej.bhs, sn, exp_cmdsn, max_cmdsn);
                write_pdu(sock, &mut rej).await?;
            }
        }
    }
    Ok(())
}

// ===== Server bind / accept loop =====

/// Per-server transport configuration. The handler carries the
/// product-specific identity (target IQN) and dispatch surface;
/// `auth` and `audit` are the two transport-time hooks the consuming
/// product wires in.
///
/// `audit` is an `Arc<dyn LoginAuditSink>` so the consuming product
/// can pick the sink at runtime (e.g. thurvsa branches on
/// `audit.enabled` between [`NoopLoginAudit`] and its real channel
/// adapter). Trait-object dispatch is one vcall per login event;
/// negligible against the I/O the handler is already doing.
pub struct ServerConfig {
    pub listen_address: String,
    pub session_manager: Arc<SessionManager>,
    /// Factory that produces a fresh [`ChapAuthenticator`] per login.
    /// `None` = no auth required (sessions accepted unauthenticated).
    /// See [`ChapAuthFactory`] for the parse-on-login semantics.
    pub auth: Option<ChapAuthFactory>,
    pub audit: Arc<dyn LoginAuditSink>,
    pub stale_session_timeout_secs: u64,
}

/// Bind, spawn the stale-session sweeper, and accept connections in
/// a loop. Each accepted connection runs in its own tokio task via
/// [`serve_connection`]. Runs forever — return value is `Err` on
/// bind / accept failure.
pub async fn run<H>(config: ServerConfig, handler: Arc<H>) -> Result<()>
where
    H: ScsiHandler + ?Sized,
{
    info!("iSCSI target starting on {}", config.listen_address);
    let listener = TcpListener::bind(&config.listen_address).await?;

    let session_mgr_sweep = Arc::clone(&config.session_manager);
    let stale_timeout = config.stale_session_timeout_secs;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            session_mgr_sweep.cleanup_stale_sessions(stale_timeout);
        }
    });

    info!("iSCSI target ready, waiting for connections...");

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                info!("New connection from {}", addr);
                let handler = Arc::clone(&handler);
                let session_manager = Arc::clone(&config.session_manager);
                let auth = config.auth.clone();
                let audit = Arc::clone(&config.audit);
                tokio::spawn(async move {
                    let peer = addr.to_string();
                    let result = serve_connection(
                        stream,
                        handler,
                        session_manager,
                        auth.as_ref(),
                        audit.as_ref(),
                        &peer,
                    )
                    .await;
                    // The owned stream was consumed by serve_connection
                    // (which split it into read/write halves and drops
                    // them on exit). TCP-level shutdown happens on Drop.
                    if let Err(e) = result {
                        if e.to_string().contains("graceful disconnect") {
                            info!("Connection from {} closed gracefully", addr);
                        } else {
                            error!("Connection error from {}: {}", addr, e);
                        }
                    }
                });
            }
            Err(e) => {
                error!("Failed to accept connection: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r2t_ttt_avoids_unsolicited_sentinel() {
        let ttt = derive_r2t_ttt(0x7FFFFFFF, 0);
        assert_ne!(ttt, 0xFFFFFFFF);
        assert_eq!(ttt, 0x80000000);
    }

    #[test]
    fn r2t_ttt_distinguishes_consecutive_r2tsns() {
        let itt = 0x12345678;
        assert_ne!(derive_r2t_ttt(itt, 0), derive_r2t_ttt(itt, 1));
        assert_ne!(derive_r2t_ttt(itt, 1), derive_r2t_ttt(itt, 2));
    }

    #[test]
    fn nop_in_response_echoes_initiator_task_tag() {
        // The initiator matches the NOP-In to its outstanding
        // NOP-Out by ITT; the reply must echo it verbatim.
        let nop_in = build_nop_in_response(0xDEAD_BEEF);
        assert_eq!(nop_in.itt, 0xDEAD_BEEF);
        assert_eq!(nop_in.opcode, 0x20);
    }

    #[test]
    fn nop_in_response_carries_reserved_target_transfer_tag() {
        // Regression: a NOP-In answering a NOP-Out must carry
        // TTT=0xFFFFFFFF. A zero TTT (the build_empty_pdu default)
        // is rejected by the Linux initiator as ISCSI_ERR_PROTO,
        // which drops the connection on every keepalive ping.
        for itt in [0u32, 1, 0x12345678, 0xFFFF_FFFF] {
            assert_eq!(
                build_nop_in_response(itt).ttt,
                0xFFFF_FFFF,
                "NOP-In for ITT=0x{itt:08x} must carry the reserved TTT"
            );
        }
    }

    #[tokio::test]
    async fn nop_in_response_serializes_reserved_ttt_on_the_wire() {
        // End-to-end: the bytes the initiator actually inspects.
        // BHS[16..20] = ITT (echoed), BHS[20..24] = TTT (reserved).
        let mut nop_in = build_nop_in_response(0x0000_0042);
        stamp_serials_for_response(&mut nop_in.bhs, 7, 3, 67);
        let mut wire = Vec::<u8>::new();
        write_pdu(&mut wire, &mut nop_in).await.unwrap();
        assert_eq!(wire[0] & 0x3F, 0x20, "opcode must be NOP-In");
        assert_eq!(&wire[16..20], &0x0000_0042u32.to_be_bytes());
        assert_eq!(
            &wire[20..24],
            &[0xFF, 0xFF, 0xFF, 0xFF],
            "TTT must be the reserved 0xFFFFFFFF"
        );
    }

    #[test]
    fn first_burst_is_bounded_by_max_recv_data_segment_length() {
        assert_eq!(FIRST_BURST_LENGTH, MAX_RECV_DATA_SEGMENT_LENGTH);
    }

    #[test]
    fn max_burst_length_at_least_max_block() {
        const _: () = assert!(MAX_BURST_LENGTH >= 16 * 1024 * 1024);
    }

    #[test]
    fn next_exp_cmdsn_immediate_no_consume() {
        let mut pdu = Pdu {
            opcode: 0x40 | 0x01,
            immediate: true,
            final_bit: false,
            total_ahs_len: 0,
            data_segment_len: 0,
            lun: [0; 8],
            itt: 0,
            ttt: 0,
            cmdsn: 42,
            expstatsn: 0,
            bhs: [0u8; 48],
            data: vec![],
        };
        assert_eq!(next_exp_cmdsn(&pdu), 42);
        pdu.immediate = false;
        assert_eq!(next_exp_cmdsn(&pdu), 43);
    }

    #[test]
    fn parse_text_kv_round_trip() {
        let mut buf = Vec::new();
        push_kv(&mut buf, "TargetName", "iqn.test");
        push_kv(&mut buf, "AuthMethod", "None");
        let parsed = parse_text_kv(&buf);
        assert_eq!(parsed.get("TargetName"), Some(&"iqn.test".to_string()));
        assert_eq!(parsed.get("AuthMethod"), Some(&"None".to_string()));
    }

    #[test]
    fn pdu_expected_xfer_len_reads_be_u32() {
        let mut pdu = Pdu {
            opcode: 0x01,
            immediate: false,
            final_bit: true,
            total_ahs_len: 0,
            data_segment_len: 0,
            lun: [0; 8],
            itt: 0,
            ttt: 0,
            cmdsn: 0,
            expstatsn: 0,
            bhs: [0u8; 48],
            data: vec![],
        };
        pdu.bhs[20..24].copy_from_slice(&12345u32.to_be_bytes());
        assert_eq!(pdu_expected_xfer_len(&pdu), 12345);
    }

    #[test]
    fn noop_audit_sink_does_nothing() {
        let sink = NoopLoginAudit;
        sink.record(LoginAuditEvent::ChapSuccess {
            peer: "x",
            initiator: None,
            user: "u",
            algorithm: "MD5",
        });
    }

    #[test]
    fn chap_auth_factory_returns_fresh_authenticator_each_call() {
        use crate::auth::{ChapAlgorithm, ChapUser};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let factory: ChapAuthFactory = Arc::new(move || {
            let n = calls_clone.fetch_add(1, Ordering::SeqCst);
            let username = format!("user-{n}");
            Ok(ChapAuthenticator::new(
                vec![ChapUser::new(username, "pw".into(), false)],
                None,
                None,
                vec![ChapAlgorithm::Sha256],
            ))
        });

        let a1 = factory().unwrap();
        let a2 = factory().unwrap();
        // Different calls see different usernames — the closure
        // actually re-runs and produces fresh state.
        assert!(a1.get_user("user-0").is_some());
        assert!(a1.get_user("user-1").is_none());
        assert!(a2.get_user("user-1").is_some());
        assert!(a2.get_user("user-0").is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn chap_auth_factory_error_propagates() {
        let factory: ChapAuthFactory =
            Arc::new(|| Err(anyhow::anyhow!("simulated config_load_failed")));
        let err = factory().unwrap_err();
        assert!(err.to_string().contains("simulated config_load_failed"));
    }
}
