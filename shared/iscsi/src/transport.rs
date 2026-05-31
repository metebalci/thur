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
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
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

/// RFC 7143 §12.12 default `MaxRecvDataSegmentLength` for any peer
/// that does not explicitly declare one during operational-parameter
/// negotiation. Used when chunking outbound Data-In PDUs against an
/// initiator that omitted the key — the target falls back to the
/// spec's 8 KiB rather than assuming the initiator can receive our
/// own [`MAX_RECV_DATA_SEGMENT_LENGTH`].
pub const DEFAULT_PEER_MAX_RECV_DATA_SEGMENT_LENGTH: u32 = 8192;
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

/// One advertised iSCSI portal: a TCP listen address paired with the
/// Target Portal Group Tag the daemon reports for it. Each portal binds
/// its own [`TcpListener`]; SendTargets emits one
/// `TargetAddress=address,tpgt` line per portal; the Login Response
/// `TargetPortalGroupTag` key carries the *arrival* portal's TPGT
/// (RFC 7143 §12.10).
///
/// Multiple portals may share one TPGT (group). Two portals with the
/// same address are rejected at `run()` — `bind(2)` would fail anyway,
/// and SendTargets would hand the initiator duplicate records.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Portal {
    pub address: String,
    pub tpgt: u16,
}

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

/// Sentinel LUN handed to the dispatcher when the host's 8-byte LUN
/// field can't map to a real logical unit. No product addresses a LUN
/// this high, so the handler's "logical unit not supported" path fires
/// (CHECK CONDITION, ASC/ASCQ 0x25/0x00) instead of aliasing.
const LUN_UNSUPPORTED: u64 = u64::MAX;

/// Decode the 8-byte SAM LUN field under the single-level "peripheral
/// device addressing" convention both products use: only `lun[1]`
/// carries the LUN number. A non-zero `lun[0]` (addressing method /
/// bus) or any non-zero byte in `lun[2..8]` is a LUN we don't address;
/// returning the sentinel keeps a host from aliasing e.g. `0x01_00`
/// onto LUN 0 (the changer) by silently dropping the high byte.
fn decode_lun(lun: &[u8; 8]) -> u64 {
    if lun[0] != 0 || lun[2..].iter().any(|&b| b != 0) {
        return LUN_UNSUPPORTED;
    }
    u64::from(lun[1])
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
/// Stamp the opcode / flag / length / tag fields into `p.bhs` for a
/// data segment of `data_len` bytes. Shared by [`write_pdu`] and
/// [`write_pdu_with_data`].
fn frame_pdu_header(p: &mut Pdu, data_len: usize) {
    p.bhs[0] = (p.bhs[0] & 0xC0) | (p.opcode & 0x3F);
    if p.immediate {
        p.bhs[0] |= 0x40;
    }
    if p.final_bit {
        p.bhs[1] |= 0x80;
    }
    p.total_ahs_len = 0;
    p.data_segment_len = data_len as u32;
    put_u24(&mut p.bhs[5..8], p.data_segment_len);
    p.bhs[16..20].copy_from_slice(&p.itt.to_be_bytes());
    p.bhs[20..24].copy_from_slice(&p.ttt.to_be_bytes());
}

async fn write_segment<W: AsyncWrite + Unpin>(sock: &mut W, data: &[u8]) -> Result<()> {
    if !data.is_empty() {
        sock.write_all(data).await?;
        let pad = (4 - (data.len() as u32 % 4)) % 4;
        if pad > 0 {
            sock.write_all(&[0u8; 3][..pad as usize]).await?;
        }
    }
    sock.flush().await?;
    Ok(())
}

pub async fn write_pdu<W: AsyncWrite + Unpin>(sock: &mut W, p: &mut Pdu) -> Result<()> {
    let data_len = p.data.len();
    frame_pdu_header(p, data_len);
    sock.write_all(&p.bhs).await?;
    write_segment(sock, &p.data).await
}

/// Write a PDU whose data segment is supplied as a borrowed slice
/// rather than living in `p.data` (which is ignored). Lets the Data-In
/// burst write each chunk straight out of the owned response buffer
/// with no per-chunk copy.
pub async fn write_pdu_with_data<W: AsyncWrite + Unpin>(
    sock: &mut W,
    p: &mut Pdu,
    data: &[u8],
) -> Result<()> {
    frame_pdu_header(p, data.len());
    sock.write_all(&p.bhs).await?;
    write_segment(sock, data).await
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
#[derive(Debug)]
pub struct LoginOutcome {
    pub is_discovery: bool,
    pub tsih: u16,
    pub cid: u16,
    pub statsn: u32,
    pub initiator_iqn: Option<String>,
    /// ISID from the login PDU BHS. Threaded into every
    /// `ScsiRequest::initiator_isid` so the SCSI surface can key
    /// persistent reservations by the stable iSCSI initiator port
    /// (IQN + ISID) rather than the ephemeral TSIH (issue #57).
    pub isid: [u8; 6],
    pub authenticated_partition: Option<String>,
    /// Volume-name set this session is admitted to (VSA only). `None`
    /// = no admission fence. Carried from CHAP login into the FFP
    /// loop, which threads it as `ScsiRequest::session_volumes` on
    /// every dispatch.
    pub authenticated_volumes: Option<Vec<String>>,
    /// Initiator-declared `MaxRecvDataSegmentLength` from operational
    /// negotiation, falling back to the RFC 7143 §12.12 default of
    /// 8192 bytes when the initiator omits the key. Bounds the data
    /// segment of each outbound Data-In PDU in the FFP loop; multi-
    /// PDU READs are chunked into back-to-back Data-Ins of at most
    /// this many bytes each, with DataSN / BufferOffset / F / S bits
    /// stamped per spec.
    pub peer_max_recv_data_segment_length: u32,
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
    authenticated_volumes: Option<Vec<String>>,
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
            authenticated_volumes: None,
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
    // Mandatory admission semantics (VSA): once CHAP succeeds,
    // `authenticated_volumes` is always `Some(_)` — never `None`. A
    // user with no `volumes` field set becomes `Some(empty)` and
    // sees no LUNs at the dispatcher, which is the safe fallback
    // under mandatory. `None` (the see-everything signal at the
    // dispatcher) is reserved for sessions that never went through
    // CHAP at all (`iscsi.auth.method: None`). VTL ignores this
    // field by construction (the SSC / SMC dispatchers don't read
    // `ScsiRequest::session_volumes`), so the always-Some shape is
    // VSA-safe + VTL-no-op.
    state.authenticated_volumes = Some(
        authenticator
            .get_user(username)
            .and_then(|u| u.volumes().map(|v| v.to_vec()))
            .unwrap_or_default(),
    );
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
    tpgt: u16,
    resp_keys: &mut Vec<u8>,
) {
    if !is_discovery {
        push_kv(resp_keys, "TargetPortalGroupTag", &tpgt.to_string());
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
///
/// `tpgt` is the Target Portal Group Tag of the portal this connection
/// arrived on — echoed back in the `TargetPortalGroupTag` response key
/// during operational-parameter negotiation (RFC 7143 §12.10 requires
/// the value match the portal the initiator dialed).
pub async fn handle_login_phase(
    sock: &mut TcpStream,
    target_iqn: &str,
    session_manager: Arc<SessionManager>,
    auth: Option<&ChapAuthFactory>,
    audit: &dyn LoginAuditSink,
    peer: &str,
    tpgt: u16,
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
    let mut peer_max_recv_data_segment_length: u32 = DEFAULT_PEER_MAX_RECV_DATA_SEGMENT_LENGTH;

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

        // Initiator-declared MaxRecvDataSegmentLength bounds the
        // data segment of each Data-In PDU we send back. RFC 7143
        // §12.12: numeric, 512..=2**24-1. Anything outside that
        // range is ignored — leaves the prior value (defaulting to
        // RFC's 8192) so a malformed key never inflates buffers.
        if let Some(v) = req_keys.get("MaxRecvDataSegmentLength")
            && let Ok(parsed) = v.parse::<u32>()
            && (512..=0x00FF_FFFF).contains(&parsed)
        {
            peer_max_recv_data_segment_length = parsed;
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
            append_opneg_response_keys(&req_keys, &params, is_discovery, tpgt, &mut resp_keys);
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
                isid,
                authenticated_partition: chap.authenticated_partition,
                authenticated_volumes: chap.authenticated_volumes,
                peer_max_recv_data_segment_length,
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
    advertised_portals: &[Portal],
    connection_tpgt: u16,
) -> Result<()> {
    let target_iqn = handler.target_iqn();
    let outcome = handle_login_phase(
        &mut sock,
        target_iqn,
        Arc::clone(&session_manager),
        auth,
        audit,
        peer,
        connection_tpgt,
    )
    .await?;
    let LoginOutcome {
        tsih,
        cid,
        mut statsn,
        initiator_iqn,
        isid,
        authenticated_partition,
        authenticated_volumes,
        peer_max_recv_data_segment_length,
        ..
    } = outcome;

    let _guard: SessionGuard<H> = SessionGuard {
        session_manager: Arc::clone(&session_manager),
        handler: Arc::clone(&handler),
        tsih,
        cid,
    };

    // PR initiator-port policy (issue #57): when the product collapses
    // the ISID, every command's nexus keys by IQN alone. Hoisted once —
    // it's a fixed per-handler setting.
    let pr_isid = if handler.pr_collapse_isid() {
        [0u8; 6]
    } else {
        isid
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

        // Discovery sessions (TSIH=0) have no SessionManager entry; the
        // per-connection `statsn` seeded from the Login Response is the
        // StatSN counter we stamp on every response PDU. Normal sessions
        // route through SessionManager so concurrent connections of the
        // same TSIH share the counter. Every responder branch below
        // (NOP-In, Text, SCSI Response/Data-In, TMF, Logout Response,
        // Reject) goes through this helper so the gate can't be missed.
        let mut next_statsn = || -> std::result::Result<u32, crate::error::IscsiError> {
            if tsih == 0 {
                let s = statsn;
                statsn = statsn.wrapping_add(1);
                Ok(s)
            } else {
                session_manager.get_and_increment_statsn(tsih, cid)
            }
        };

        // Stamp a response PDU's serial fields (StatSN from
        // `next_statsn`, ExpCmdSN / MaxCmdSN derived from the request
        // it answers) and write it. Every single-PDU responder arm
        // below goes through this so a serial-field change is one edit,
        // not eight. `$req` is the incoming PDU being answered. (The
        // chunked Data-In path stamps inline — it spreads DataSN across
        // multiple PDUs with per-PDU S-bit logic the macro can't model.)
        macro_rules! stamp_and_write {
            ($resp:expr, $req:expr) => {{
                let exp_cmdsn = next_exp_cmdsn($req);
                let max_cmdsn = exp_cmdsn.wrapping_add(ADVERTISED_CMDSN_WINDOW);
                let sn = next_statsn()?;
                stamp_serials_for_response(&mut $resp.bhs, sn, exp_cmdsn, max_cmdsn);
                write_pdu(sock, &mut $resp).await?;
            }};
        }

        // Build a Reject PDU (opcode 0x3F, F=1) with the given
        // InitiatorTaskTag and reason code (`bhs[2]`). Protocol-error
        // rejects use ITT 0xFFFFFFFF (unsolicited); a per-command reject
        // (e.g. unsupported opcode) echoes the command's ITT.
        let build_reject = |itt: u32, reason: u8| {
            let mut rej = build_empty_pdu(0x3F, true, true);
            rej.itt = itt;
            rej.bhs[2] = reason;
            rej
        };

        match pdu.opcode & 0x3F {
            0x00 => {
                // NOP-Out ping; answer with a NOP-In. Legal in
                // discovery sessions (RFC 7143 §6.1).
                let mut nop_in = build_nop_in_response(pdu.itt);
                stamp_and_write!(nop_in, &pdu);
            }
            0x04 => {
                // Text Request — SendTargets discovery
                let req_keys = parse_text_kv(&pdu.data);
                let mut tx = build_empty_pdu(0x24, false, true);
                tx.itt = pdu.itt;

                if let Some(st) = req_keys.get("SendTargets") {
                    // Legacy fallback: matches the pre-multi-portal
                    // behavior on `sock.local_addr()` failure
                    // (vanishingly rare after an accepted connection).
                    const FALLBACK_LOCAL: SocketAddr =
                        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3260));
                    let local = sock.local_addr().unwrap_or(FALLBACK_LOCAL);
                    let mut lines = Vec::<u8>::new();
                    if st.eq_ignore_ascii_case("All") || st == target_iqn {
                        push_kv(&mut lines, "TargetName", target_iqn);
                        for (addr, tpgt) in build_target_addresses(advertised_portals, local) {
                            push_kv(&mut lines, "TargetAddress", &format!("{},{}", addr, tpgt));
                        }
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
                        connection_tpgt,
                        MAX_RECV_DATA_SEGMENT_LENGTH,
                        MAX_BURST_LENGTH,
                        FIRST_BURST_LENGTH
                    )
                    .into_bytes();
                    tx.data = keys;
                }
                let _ = format_text_data(&tx.data);
                stamp_and_write!(tx, &pdu);
            }
            0x01 => {
                // SCSI Command. Illegal in discovery sessions
                // (RFC 7143 §6.1) — reject with a typed
                // protocol-error Reject rather than dispatching into
                // the handler with no LUN context.
                if tsih == 0 {
                    tracing::warn!(
                        "SCSI Command on discovery session from {peer} — protocol violation"
                    );
                    let mut rej = build_reject(0xFFFFFFFF, 0x04);
                    stamp_and_write!(rej, &pdu);
                    continue;
                }

                // Drain Data-Out (R2T loop) before dispatch — the
                // handler sees a complete payload. The reader
                // pre-registered an ITT-keyed Data-Out route iff the
                // W-bit was set; drop it after the burst completes
                // (success or error).
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

                let lun = decode_lun(&pdu.lun);
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
                    initiator_isid: pr_isid,
                    peer,
                    session_partition: authenticated_partition.as_deref(),
                    session_volumes: authenticated_volumes.as_deref(),
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
                    let total_len = data_in.len();
                    let residual = edtl.saturating_sub(total_len as u32);

                    // RFC 7143 §11.7: chunk the Data-In stream into
                    // PDUs of at most peer-declared MaxRecvData-
                    // SegmentLength bytes, stamping DataSN (0, 1,
                    // 2, …) and BufferOffset on every PDU. Only the
                    // final PDU carries F=1 + S=1 + the SCSI status
                    // byte; non-final PDUs leave the StatSN field
                    // zero (S=0 makes it reserved). A single PDU
                    // smaller than the cap collapses to the
                    // historical one-PDU path.
                    let chunk_size = peer_max_recv_data_segment_length as usize;
                    let status_code = resp.status.code();
                    let mut cursor = 0usize;
                    let mut data_sn: u32 = 0;
                    while cursor < total_len {
                        let end = total_len.min(cursor + chunk_size);
                        let is_last = end == total_len;
                        let mut din = build_empty_pdu(0x25, true, is_last);
                        din.itt = pdu.itt;
                        din.ttt = 0xFFFFFFFF;
                        if is_last {
                            din.bhs[1] = 0x80 | 0x01; // F | S
                            if residual > 0 {
                                din.bhs[1] |= 0x02; // U
                            }
                            din.bhs[3] = status_code;
                            din.bhs[44..48].copy_from_slice(&residual.to_be_bytes());
                        }
                        din.bhs[36..40].copy_from_slice(&data_sn.to_be_bytes());
                        din.bhs[40..44].copy_from_slice(&(cursor as u32).to_be_bytes());
                        if is_last {
                            let sn = next_statsn()?;
                            stamp_serials_for_response(&mut din.bhs, sn, exp_cmdsn, max_cmdsn);
                        } else {
                            // Non-final Data-In: StatSN reserved
                            // (S=0). Still stamp ExpCmdSN /
                            // MaxCmdSN so the initiator's CmdSN
                            // window stays open while it gathers
                            // the burst.
                            din.bhs[28..32].copy_from_slice(&exp_cmdsn.to_be_bytes());
                            din.bhs[32..36].copy_from_slice(&max_cmdsn.to_be_bytes());
                        }
                        write_pdu_with_data(sock, &mut din, &data_in[cursor..end]).await?;
                        data_sn = data_sn.wrapping_add(1);
                        cursor = end;
                    }
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
                    stamp_and_write!(sresp, &pdu);
                }
            }
            0x02 => {
                // Task Management. Illegal in discovery sessions
                // (RFC 7143 §6.1) — reject with a typed protocol-
                // error Reject rather than fabricating a "function
                // complete" TMF Response without session state.
                if tsih == 0 {
                    tracing::warn!(
                        "Task Management on discovery session from {peer} — protocol violation"
                    );
                    let mut rej = build_reject(0xFFFFFFFF, 0x04);
                    stamp_and_write!(rej, &pdu);
                    continue;
                }
                let mut tmf = build_empty_pdu(0x22, true, true);
                tmf.itt = pdu.itt;
                tmf.bhs[2] = 0x00;
                stamp_and_write!(tmf, &pdu);
            }
            0x06 => {
                // Logout — connection closed successfully. Legal in
                // both normal and discovery sessions (RFC 7143 §6.1);
                // the latter is how libiscsi tears down `iscsi-ls`.
                let mut logout_resp = build_empty_pdu(0x26, true, true);
                logout_resp.itt = pdu.itt;
                logout_resp.bhs[2] = 0x00;
                stamp_and_write!(logout_resp, &pdu);
                break;
            }
            0x05 => {
                // Stray Data-Out at the FFP top — protocol error.
                tracing::warn!(
                    "Stray Data-Out PDU ITT=0x{:08x} TTT=0x{:08x}",
                    pdu.itt,
                    pdu.ttt
                );
                let mut rej = build_reject(0xFFFFFFFF, 0x04); // Protocol error
                stamp_and_write!(rej, &pdu);
            }
            0xFF => break,
            _ => {
                error!(
                    "Unsupported opcode: 0x{:02x} ({})",
                    pdu.opcode & 0x3F,
                    opcode_name(pdu.opcode)
                );
                let mut rej = build_reject(pdu.itt, 0x09); // Command not supported
                stamp_and_write!(rej, &pdu);
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
/// Build the list of `(TargetAddress, TPGT)` payloads to emit for
/// SendTargets, one per advertised portal. Wildcard binds
/// (`0.0.0.0:*`, `[::]:*`) substitute the connection's actual local IP
/// — without this, an initiator would receive an unusable
/// `0.0.0.0:3260` line. Concrete addresses are emitted literally.
/// Duplicates after substitution are dropped so an operator who lists
/// `["0.0.0.0:3260", "192.0.2.5:3260"]` doesn't see the same record
/// twice; when a wildcard portal collapses to the same line as a
/// concrete one with a different TPGT, the first entry wins (the
/// dedup key is the address, not the `(address, tpgt)` pair — two
/// `TargetAddress` lines with the same `ip:port` and different TPGTs
/// would confuse the initiator).
fn build_target_addresses(advertised: &[Portal], local: SocketAddr) -> Vec<(String, u16)> {
    let mut out: Vec<(String, u16)> = Vec::with_capacity(advertised.len());
    for portal in advertised {
        let line = match portal.address.parse::<SocketAddr>() {
            Ok(sa) if sa.ip().is_unspecified() => {
                SocketAddr::new(local.ip(), sa.port()).to_string()
            }
            Ok(sa) => sa.to_string(),
            Err(_) => portal.address.clone(),
        };
        if !out.iter().any(|(a, _)| a == &line) {
            out.push((line, portal.tpgt));
        }
    }
    out
}

/// Which iSCSI initiator-port identity the SCSI layer keys persistent
/// reservations by (issue #57). The ISID enters the SCSI nexus only via
/// [`ScsiRequest::initiator_isid`]; this selects whether the real ISID
/// or a fixed constant reaches it.
///
/// - [`IqnIsid`](Self::IqnIsid) (default): the full, spec-literal iSCSI
///   initiator port — initiator IQN + ISID. Models per-path
///   (`mpathpersist`-style) registration; a host reclaims a reservation
///   across a reconnect only if it reuses its ISID (Windows / VMware /
///   session reinstatement do; open-iscsi mints a fresh ISID per
///   manual login).
/// - [`Iqn`](Self::Iqn): collapse the ISID to a fixed safe constant
///   (all-zero) before it reaches the SCSI layer, so a registrant keys
///   by IQN alone. A host reclaims its reservation across any
///   reconnect / target restart regardless of ISID churn, at the cost
///   of treating all of that host's concurrent sessions as one
///   registrant. Opt-in via the `iscsi.reservations.initiator_port`
///   conffile key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrInitiatorPort {
    #[default]
    IqnIsid,
    Iqn,
}

impl PrInitiatorPort {
    /// Whether to zero the ISID before it reaches the SCSI nexus.
    pub fn collapse_isid(self) -> bool {
        matches!(self, Self::Iqn)
    }
}

/// The `iscsi.reservations:` conffile block (shared by both products).
/// Optional — when omitted, the defaults apply (full IQN + ISID
/// initiator port).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ReservationSettings {
    /// Which initiator-port identity persistent reservations key by.
    /// See [`PrInitiatorPort`].
    #[serde(default)]
    pub initiator_port: PrInitiatorPort,
}

pub struct ServerConfig {
    /// One or more iSCSI TCP portals to bind. Each entry binds its own
    /// [`TcpListener`]; SendTargets discovery enumerates every entry
    /// (see [`build_target_addresses`]) and the Login Response
    /// `TargetPortalGroupTag` for an arriving connection carries that
    /// portal's TPGT. Must contain at least one portal.
    pub listen_portals: Vec<Portal>,
    pub session_manager: Arc<SessionManager>,
    /// Factory that produces a fresh [`ChapAuthenticator`] per login.
    /// `None` = no auth required (sessions accepted unauthenticated).
    /// See [`ChapAuthFactory`] for the parse-on-login semantics.
    pub auth: Option<ChapAuthFactory>,
    pub audit: Arc<dyn LoginAuditSink>,
    pub stale_session_timeout_secs: u64,
}

/// Bind every configured portal, spawn the stale-session sweeper,
/// and run one accept loop per listener. Each accepted connection
/// runs in its own tokio task via [`serve_connection`]. Runs forever
/// — return value is `Err` on bind / accept failure on any listener.
///
/// Rejects duplicate `address` entries in `listen_portals` at boot:
/// `bind(2)` would fail anyway, and the same `ip:port` advertised
/// twice (regardless of TPGT) would hand the initiator records it
/// can't disambiguate. Multiple portals sharing one TPGT (a group)
/// is legal.
pub async fn run<H>(config: ServerConfig, handler: Arc<H>) -> Result<()>
where
    H: ScsiHandler + ?Sized,
{
    if config.listen_portals.is_empty() {
        return Err(anyhow!(
            "iscsi: ServerConfig.listen_portals must contain at least one entry"
        ));
    }

    {
        let mut seen: HashMap<&str, u16> = HashMap::new();
        for p in &config.listen_portals {
            if let Some(prev_tpgt) = seen.insert(p.address.as_str(), p.tpgt) {
                return Err(anyhow!(
                    "iscsi: duplicate listen address {} (TPGT {} and {}); \
                     each address must appear once",
                    p.address,
                    prev_tpgt,
                    p.tpgt
                ));
            }
        }
    }

    info!(
        "iSCSI target starting on {}",
        config
            .listen_portals
            .iter()
            .map(|p| format!("{},tpgt={}", p.address, p.tpgt))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let mut listeners = Vec::with_capacity(config.listen_portals.len());
    for portal in &config.listen_portals {
        let listener = TcpListener::bind(&portal.address)
            .await
            .map_err(|e| anyhow!("iscsi: bind {}: {}", portal.address, e))?;
        listeners.push((listener, portal.tpgt));
    }

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

    let advertised: Arc<Vec<Portal>> = Arc::new(config.listen_portals.clone());
    let mut accepts = tokio::task::JoinSet::new();
    for (listener, tpgt) in listeners {
        let handler = Arc::clone(&handler);
        let session_manager = Arc::clone(&config.session_manager);
        let auth = config.auth.clone();
        let audit = Arc::clone(&config.audit);
        let advertised = Arc::clone(&advertised);
        accepts.spawn(async move {
            accept_loop::<H>(
                listener,
                tpgt,
                handler,
                session_manager,
                auth,
                audit,
                advertised,
            )
            .await
        });
    }

    while let Some(joined) = accepts.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(anyhow!("iscsi accept loop joined with error: {}", e)),
        }
    }
    Ok(())
}

async fn accept_loop<H>(
    listener: TcpListener,
    tpgt: u16,
    handler: Arc<H>,
    session_manager: Arc<SessionManager>,
    auth: Option<ChapAuthFactory>,
    audit: Arc<dyn LoginAuditSink>,
    advertised: Arc<Vec<Portal>>,
) -> Result<()>
where
    H: ScsiHandler + ?Sized,
{
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                info!("New connection from {} (TPGT={})", addr, tpgt);
                let handler = Arc::clone(&handler);
                let session_manager = Arc::clone(&session_manager);
                let auth = auth.clone();
                let audit = Arc::clone(&audit);
                let advertised = Arc::clone(&advertised);
                tokio::spawn(async move {
                    let peer = addr.to_string();
                    let result = serve_connection(
                        stream,
                        handler,
                        session_manager,
                        auth.as_ref(),
                        audit.as_ref(),
                        &peer,
                        &advertised,
                        tpgt,
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
    use crate::handler::ScsiResponse;

    #[test]
    fn r2t_ttt_avoids_unsolicited_sentinel() {
        let ttt = derive_r2t_ttt(0x7FFFFFFF, 0);
        assert_ne!(ttt, 0xFFFFFFFF);
        assert_eq!(ttt, 0x80000000);
    }

    #[test]
    fn pr_initiator_port_default_and_collapse() {
        // Default keeps the full (IQN, ISID) port.
        assert_eq!(PrInitiatorPort::default(), PrInitiatorPort::IqnIsid);
        assert!(!PrInitiatorPort::IqnIsid.collapse_isid());
        assert!(PrInitiatorPort::Iqn.collapse_isid());
    }

    #[test]
    fn reservation_settings_parse_kebab_case() {
        // Conffile values are kebab-case ("iqn" / "iqn-isid"); an
        // omitted block / field defaults to the full initiator port.
        // (serde_json exercises the same rename as the YAML conffile.)
        let s: ReservationSettings = serde_json::from_str(r#"{"initiator_port":"iqn"}"#).unwrap();
        assert_eq!(s.initiator_port, PrInitiatorPort::Iqn);
        let s: ReservationSettings =
            serde_json::from_str(r#"{"initiator_port":"iqn-isid"}"#).unwrap();
        assert_eq!(s.initiator_port, PrInitiatorPort::IqnIsid);
        let s: ReservationSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.initiator_port, PrInitiatorPort::IqnIsid);
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
    fn decode_lun_reads_single_byte() {
        assert_eq!(decode_lun(&[0, 0, 0, 0, 0, 0, 0, 0]), 0);
        assert_eq!(decode_lun(&[0, 5, 0, 0, 0, 0, 0, 0]), 5);
        assert_eq!(decode_lun(&[0, 255, 0, 0, 0, 0, 0, 0]), 255);
    }

    #[test]
    fn decode_lun_rejects_high_byte_instead_of_aliasing() {
        // 0x01_00 must NOT alias onto LUN 0 (the changer) — it has no
        // real LUN, so it decodes to the unsupported sentinel.
        assert_eq!(decode_lun(&[0x01, 0x00, 0, 0, 0, 0, 0, 0]), LUN_UNSUPPORTED);
        // A non-zero byte anywhere in lun[2..8] is equally unaddressable.
        assert_eq!(decode_lun(&[0, 1, 0, 0, 0, 0, 0, 1]), LUN_UNSUPPORTED);
        assert_eq!(decode_lun(&[0, 0, 0, 0, 0, 0, 0, 1]), LUN_UNSUPPORTED);
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

    // ===== Pure-function coverage =====

    #[test]
    fn u24_and_put_u24_round_trip() {
        let mut buf = [0u8; 3];
        for v in [0u32, 1, 255, 256, 0x00FF_FFFF, 0x0012_3456] {
            put_u24(&mut buf, v);
            assert_eq!(u24(&buf), v, "u24 round-trip for {v}");
        }
    }

    #[test]
    fn opcode_name_covers_initiator_and_target_opcodes() {
        // High bits are masked off — 0x40|0x01 still names "SCSI Command".
        assert_eq!(opcode_name(0x40 | 0x01), "SCSI Command");
        assert_eq!(opcode_name(0x00), "NOP-Out");
        assert_eq!(opcode_name(0x03), "Login Req");
        assert_eq!(opcode_name(0x05), "Data-Out");
        assert_eq!(opcode_name(0x06), "Logout Req");
        assert_eq!(opcode_name(0x20), "NOP-In");
        assert_eq!(opcode_name(0x21), "SCSI Resp");
        assert_eq!(opcode_name(0x25), "Data-In");
        assert_eq!(opcode_name(0x3F), "Reject");
        assert_eq!(opcode_name(0x1A), "Unknown");
    }

    #[test]
    fn preview_truncates_and_marks_overflow() {
        let short = preview(&[0xDE, 0xAD], 8);
        assert_eq!(short, "de ad");
        let long = preview(&[0x11, 0x22, 0x33, 0x44], 2);
        assert!(long.starts_with("11 22"));
        assert!(long.ends_with('…'));
        assert_eq!(preview(&[], 4), "");
    }

    #[test]
    fn format_text_data_joins_nul_separated_pairs() {
        let data = b"TargetName=iqn.x\0AuthMethod=None\0";
        assert_eq!(format_text_data(data), "TargetName=iqn.x, AuthMethod=None");
        // Empty segments between NULs are dropped.
        assert_eq!(format_text_data(b"\0\0A=1\0\0"), "A=1");
    }

    #[test]
    fn parse_text_kv_ignores_segments_without_equals() {
        let parsed = parse_text_kv(b"Key=Val\0junk\0K2=V2\0");
        assert_eq!(parsed.get("Key"), Some(&"Val".to_string()));
        assert_eq!(parsed.get("K2"), Some(&"V2".to_string()));
        assert_eq!(parsed.len(), 2, "the 'junk' segment carries no '='");
    }

    #[test]
    fn parse_text_kv_keeps_empty_value() {
        let parsed = parse_text_kv(b"Empty=\0");
        assert_eq!(parsed.get("Empty"), Some(&String::new()));
    }

    #[test]
    fn next_exp_cmdsn_wraps_at_u32_max() {
        let mut pdu = build_empty_pdu(0x01, false, false);
        pdu.cmdsn = u32::MAX;
        assert_eq!(next_exp_cmdsn(&pdu), 0, "non-immediate CmdSN wraps");
        pdu.immediate = true;
        assert_eq!(next_exp_cmdsn(&pdu), u32::MAX, "immediate echoes verbatim");
    }

    #[test]
    fn stamp_serials_writes_three_be_u32s() {
        let mut bhs = [0u8; 48];
        stamp_serials_for_response(&mut bhs, 0x1111_2222, 0x3333_4444, 0x5555_6666);
        assert_eq!(&bhs[24..28], &0x1111_2222u32.to_be_bytes());
        assert_eq!(&bhs[28..32], &0x3333_4444u32.to_be_bytes());
        assert_eq!(&bhs[32..36], &0x5555_6666u32.to_be_bytes());
    }

    #[test]
    fn derive_r2t_ttt_always_sets_high_bit() {
        for (itt, r2tsn) in [(0u32, 0u32), (1, 5), (0xFFFF_FFFF, 7), (0x7FFF_FFFF, 0)] {
            let ttt = derive_r2t_ttt(itt, r2tsn);
            assert_ne!(ttt, 0xFFFF_FFFF, "TTT must avoid the unsolicited sentinel");
            assert_ne!(ttt, 0, "TTT must not collide with build_empty_pdu default");
        }
    }

    #[test]
    fn build_empty_pdu_sets_opcode_and_flags() {
        let p = build_empty_pdu(0x23, false, true);
        assert_eq!(p.opcode, 0x23);
        assert!(!p.immediate);
        assert!(p.final_bit);
        assert!(p.data.is_empty());
        assert_eq!(p.bhs, [0u8; 48]);
    }

    #[test]
    fn session_params_default_matches_wire_constants() {
        let p = SessionParams::default();
        assert_eq!(p.max_recv_data_segment_length, MAX_RECV_DATA_SEGMENT_LENGTH);
        assert_eq!(p.max_burst_length, MAX_BURST_LENGTH);
        assert_eq!(p.first_burst_length, FIRST_BURST_LENGTH);
        assert!(p.immediate_data);
        assert!(!p.initial_r2t);
        assert_eq!(p.max_connections, 1);
    }

    #[test]
    fn append_opneg_response_keys_echoes_offered_keys() {
        let mut req = HashMap::new();
        req.insert("HeaderDigest".to_string(), "CRC32C,None".to_string());
        req.insert("DataDigest".to_string(), "CRC32C,None".to_string());
        req.insert("OFMarker".to_string(), "Yes".to_string());
        req.insert("IFMarker".to_string(), "Yes".to_string());
        req.insert("ErrorRecoveryLevel".to_string(), "0".to_string());
        req.insert("DefaultTime2Wait".to_string(), "5".to_string());
        let mut out = Vec::new();
        append_opneg_response_keys(&req, &SessionParams::default(), false, 1, &mut out);
        let kv = parse_text_kv(&out);
        // Digests / markers are pinned to the safe target-side value.
        assert_eq!(kv.get("HeaderDigest"), Some(&"None".to_string()));
        assert_eq!(kv.get("DataDigest"), Some(&"None".to_string()));
        assert_eq!(kv.get("OFMarker"), Some(&"No".to_string()));
        assert_eq!(kv.get("IFMarker"), Some(&"No".to_string()));
        assert_eq!(kv.get("ErrorRecoveryLevel"), Some(&"0".to_string()));
        // Offered DefaultTime2Wait is echoed back.
        assert_eq!(kv.get("DefaultTime2Wait"), Some(&"5".to_string()));
        // Operational-session-only keys are present for non-discovery.
        assert_eq!(kv.get("ImmediateData"), Some(&"Yes".to_string()));
        assert_eq!(kv.get("InitialR2T"), Some(&"No".to_string()));
        assert!(kv.contains_key("TargetPortalGroupTag"));
        assert!(kv.contains_key("MaxConnections"));
    }

    #[test]
    fn append_opneg_response_keys_discovery_omits_session_keys() {
        let req = HashMap::new();
        let mut out = Vec::new();
        append_opneg_response_keys(&req, &SessionParams::default(), true, 1, &mut out);
        let kv = parse_text_kv(&out);
        // Discovery sessions get no TPGT / ImmediateData / MaxConnections.
        assert!(!kv.contains_key("TargetPortalGroupTag"));
        assert!(!kv.contains_key("ImmediateData"));
        assert!(!kv.contains_key("MaxConnections"));
        // Default time2* are emitted when not offered.
        assert!(kv.contains_key("DefaultTime2Wait"));
        assert!(kv.contains_key("DefaultTime2Retain"));
        assert!(kv.contains_key("MaxRecvDataSegmentLength"));
    }

    // ===== read_pdu / write_pdu round-trips =====

    #[tokio::test]
    async fn write_then_read_pdu_round_trips_header_and_data() {
        let mut p = build_empty_pdu(0x01, false, true);
        p.itt = 0xABCD_1234;
        p.ttt = 0x5678_9ABC;
        p.lun = [0, 3, 0, 0, 0, 0, 0, 0];
        p.bhs[8..16].copy_from_slice(&p.lun);
        p.bhs[24..28].copy_from_slice(&77u32.to_be_bytes()); // CmdSN
        p.data = b"hello-iscsi".to_vec(); // 11 bytes, needs 1 pad byte
        let mut wire = Vec::new();
        write_pdu(&mut wire, &mut p).await.unwrap();
        // 48 BHS + 11 data + 1 pad = 60, a multiple of 4.
        assert_eq!(wire.len(), 60);

        let mut cursor = std::io::Cursor::new(wire);
        let parsed = read_pdu(&mut cursor).await.unwrap();
        assert_eq!(parsed.opcode, 0x01);
        assert!(parsed.final_bit);
        assert_eq!(parsed.itt, 0xABCD_1234);
        assert_eq!(parsed.ttt, 0x5678_9ABC);
        assert_eq!(parsed.lun, [0, 3, 0, 0, 0, 0, 0, 0]);
        assert_eq!(parsed.cmdsn, 77);
        assert_eq!(parsed.data, b"hello-iscsi");
        assert_eq!(parsed.data_segment_len, 11);
    }

    #[tokio::test]
    async fn read_pdu_eof_is_graceful_disconnect() {
        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        let err = read_pdu(&mut empty).await.unwrap_err();
        assert!(err.to_string().contains("graceful disconnect"));
    }

    #[tokio::test]
    async fn read_pdu_truncated_bhs_is_graceful_disconnect() {
        // Only 10 bytes of the 48-byte BHS — read_exact hits EOF.
        let mut partial = std::io::Cursor::new(vec![0u8; 10]);
        let err = read_pdu(&mut partial).await.unwrap_err();
        assert!(err.to_string().contains("graceful disconnect"));
    }

    #[tokio::test]
    async fn read_pdu_rejects_oversized_data_segment() {
        // Hand-craft a BHS whose u24 DataSegmentLength exceeds the cap.
        let mut bhs = [0u8; 48];
        bhs[0] = 0x01;
        let oversize = MAX_RECV_DATA_SEGMENT_LENGTH + 1;
        put_u24(&mut bhs[5..8], oversize);
        let mut cursor = std::io::Cursor::new(bhs.to_vec());
        let err = read_pdu(&mut cursor).await.unwrap_err();
        assert!(
            err.to_string().contains("exceeds MaxRecvDataSegmentLength"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn read_pdu_skips_additional_header_segment() {
        // total_ahs_len = 1 → 4 AHS bytes follow the BHS, then data.
        let mut bhs = [0u8; 48];
        bhs[0] = 0x01;
        bhs[4] = 1; // TotalAHSLength in 4-byte units
        put_u24(&mut bhs[5..8], 4); // 4 data bytes
        let mut wire = bhs.to_vec();
        wire.extend_from_slice(&[0xAA, 0xAA, 0xAA, 0xAA]); // AHS
        wire.extend_from_slice(&[1, 2, 3, 4]); // data (4, no pad)
        let mut cursor = std::io::Cursor::new(wire);
        let parsed = read_pdu(&mut cursor).await.unwrap();
        assert_eq!(parsed.total_ahs_len, 1);
        assert_eq!(parsed.data, [1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn read_pdu_truncated_data_segment_is_graceful_disconnect() {
        let mut bhs = [0u8; 48];
        bhs[0] = 0x01;
        put_u24(&mut bhs[5..8], 16); // claims 16 data bytes
        let mut wire = bhs.to_vec();
        wire.extend_from_slice(&[0u8; 4]); // only 4 supplied
        let mut cursor = std::io::Cursor::new(wire);
        let err = read_pdu(&mut cursor).await.unwrap_err();
        assert!(err.to_string().contains("graceful disconnect"));
    }

    #[tokio::test]
    async fn write_pdu_no_data_emits_bare_bhs() {
        let mut p = build_empty_pdu(0x20, true, true);
        p.itt = 1;
        let mut wire = Vec::new();
        write_pdu(&mut wire, &mut p).await.unwrap();
        assert_eq!(wire.len(), 48);
        // Immediate bit (0x40) and final bit (0x80) are stamped.
        assert_eq!(wire[0] & 0x40, 0x40);
        assert_eq!(wire[1] & 0x80, 0x80);
    }

    // ===== send_login_rejection =====

    #[tokio::test]
    async fn send_login_rejection_emits_status_class_and_detail() {
        // Drive send_login_rejection over a TCP pair; the rejection PDU
        // is a Login Response (0x23) with status class/detail in BHS
        // bytes 36/37.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let req = build_empty_pdu(0x03, false, false);
            let mut statsn = 5u32;
            send_login_rejection(
                &mut sock,
                &req,
                0,
                &[1, 2, 3, 4, 5, 6],
                &mut statsn,
                0x02,
                0x01,
            )
            .await
            .unwrap();
            statsn
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        let resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(resp.opcode, 0x23, "Login Response opcode");
        assert_eq!(resp.bhs[36], 0x02, "status class = authentication failure");
        assert_eq!(resp.bhs[37], 0x01, "status detail");
        let final_statsn = server.await.unwrap();
        assert_eq!(final_statsn, 6, "statsn advanced once");
    }

    // ===== collect_write_data: unsolicited + R2T phases =====

    /// Build a Data-Out PDU (opcode 0x05) for the R2T loop tests.
    fn make_data_out(
        itt: u32,
        ttt: u32,
        buffer_offset: u32,
        payload: &[u8],
        final_bit: bool,
    ) -> Pdu {
        let mut p = build_empty_pdu(0x05, false, final_bit);
        p.itt = itt;
        p.ttt = ttt;
        p.data = payload.to_vec();
        p.bhs[40..44].copy_from_slice(&buffer_offset.to_be_bytes());
        p
    }

    /// Spin up a session + connection so collect_write_data can read
    /// the StatSN for its R2T PDUs.
    fn session_with_connection() -> (Arc<SessionManager>, u16, u16) {
        let mgr = Arc::new(SessionManager::new());
        let tsih = mgr.create_session([9, 9, 9, 9, 9, 9]);
        mgr.add_connection(tsih, 0, MAX_RECV_DATA_SEGMENT_LENGTH)
            .expect("add_connection");
        (mgr, tsih, 0)
    }

    #[tokio::test]
    async fn collect_write_data_unsolicited_burst_then_done() {
        // cmd carries no immediate data, final_bit clear → phase 1
        // drains one unsolicited Data-Out that completes the EDTL.
        let (mgr, tsih, cid) = session_with_connection();
        let mut cmd = build_empty_pdu(0x01, false, false);
        cmd.itt = 0x1000;
        let edtl = 8u32;

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Pdu>(4);
        tx.send(make_data_out(
            0x1000,
            0xFFFF_FFFF,
            0,
            &[1, 2, 3, 4, 5, 6, 7, 8],
            true,
        ))
        .await
        .unwrap();
        drop(tx);

        let mut sink = Vec::new();
        collect_write_data(&mut sink, &mut cmd, edtl, &mgr, tsih, cid, &mut rx)
            .await
            .unwrap();
        assert_eq!(cmd.data, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[tokio::test]
    async fn collect_write_data_r2t_solicited_burst() {
        // cmd.final_bit set so phase 1 is skipped; phase 2 issues one
        // R2T and the matching Data-Out completes the burst.
        let (mgr, tsih, cid) = session_with_connection();
        let mut cmd = build_empty_pdu(0x01, false, true);
        cmd.itt = 0x2000;
        let edtl = 4u32;
        let ttt = derive_r2t_ttt(cmd.itt, 0);

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Pdu>(4);
        tx.send(make_data_out(
            0x2000,
            ttt,
            0,
            &[0xDE, 0xAD, 0xBE, 0xEF],
            true,
        ))
        .await
        .unwrap();
        drop(tx);

        let mut sink = Vec::new();
        collect_write_data(&mut sink, &mut cmd, edtl, &mgr, tsih, cid, &mut rx)
            .await
            .unwrap();
        assert_eq!(cmd.data, [0xDE, 0xAD, 0xBE, 0xEF]);
        // An R2T PDU (opcode 0x31) was written to the sink.
        let mut cursor = std::io::Cursor::new(sink);
        let r2t = read_pdu(&mut cursor).await.unwrap();
        assert_eq!(r2t.opcode & 0x3F, 0x31, "an R2T was sent");
        assert_eq!(r2t.itt, 0x2000);
    }

    #[tokio::test]
    async fn collect_write_data_rejects_wrong_opcode() {
        let (mgr, tsih, cid) = session_with_connection();
        let mut cmd = build_empty_pdu(0x01, false, false);
        cmd.itt = 0x3000;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Pdu>(4);
        // A SCSI Command (0x01) instead of Data-Out (0x05).
        let mut wrong = build_empty_pdu(0x01, false, true);
        wrong.itt = 0x3000;
        tx.send(wrong).await.unwrap();
        drop(tx);
        let mut sink = Vec::new();
        let err = collect_write_data(&mut sink, &mut cmd, 8, &mgr, tsih, cid, &mut rx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("expected unsolicited Data-Out"));
    }

    #[tokio::test]
    async fn collect_write_data_rejects_itt_mismatch() {
        let (mgr, tsih, cid) = session_with_connection();
        let mut cmd = build_empty_pdu(0x01, false, false);
        cmd.itt = 0x4000;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Pdu>(4);
        tx.send(make_data_out(0xBADD, 0xFFFF_FFFF, 0, &[0u8; 4], true))
            .await
            .unwrap();
        drop(tx);
        let mut sink = Vec::new();
        let err = collect_write_data(&mut sink, &mut cmd, 8, &mgr, tsih, cid, &mut rx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ITT mismatch"));
    }

    #[tokio::test]
    async fn collect_write_data_rejects_unsolicited_non_default_ttt() {
        let (mgr, tsih, cid) = session_with_connection();
        let mut cmd = build_empty_pdu(0x01, false, false);
        cmd.itt = 0x5000;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Pdu>(4);
        // Unsolicited Data-Out must carry TTT=0xFFFFFFFF.
        tx.send(make_data_out(0x5000, 0x1234, 0, &[0u8; 4], true))
            .await
            .unwrap();
        drop(tx);
        let mut sink = Vec::new();
        let err = collect_write_data(&mut sink, &mut cmd, 8, &mgr, tsih, cid, &mut rx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("non-default TTT"));
    }

    #[tokio::test]
    async fn collect_write_data_rejects_buffer_offset_gap() {
        let (mgr, tsih, cid) = session_with_connection();
        let mut cmd = build_empty_pdu(0x01, false, false);
        cmd.itt = 0x6000;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Pdu>(4);
        // BufferOffset 4 but nothing accumulated yet.
        tx.send(make_data_out(0x6000, 0xFFFF_FFFF, 4, &[0u8; 4], true))
            .await
            .unwrap();
        drop(tx);
        let mut sink = Vec::new();
        let err = collect_write_data(&mut sink, &mut cmd, 8, &mgr, tsih, cid, &mut rx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("BufferOffset"));
    }

    #[tokio::test]
    async fn collect_write_data_rejects_edtl_overrun() {
        let (mgr, tsih, cid) = session_with_connection();
        let mut cmd = build_empty_pdu(0x01, false, false);
        cmd.itt = 0x7000;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Pdu>(4);
        // 8 bytes when EDTL is only 4.
        tx.send(make_data_out(0x7000, 0xFFFF_FFFF, 0, &[0u8; 8], true))
            .await
            .unwrap();
        drop(tx);
        let mut sink = Vec::new();
        let err = collect_write_data(&mut sink, &mut cmd, 4, &mgr, tsih, cid, &mut rx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("overruns EDTL"));
    }

    #[tokio::test]
    async fn collect_write_data_channel_close_is_error() {
        let (mgr, tsih, cid) = session_with_connection();
        let mut cmd = build_empty_pdu(0x01, false, false);
        cmd.itt = 0x8000;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Pdu>(4);
        drop(tx); // reader "closed" before any Data-Out arrived
        let mut sink = Vec::new();
        let err = collect_write_data(&mut sink, &mut cmd, 8, &mgr, tsih, cid, &mut rx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("PDU reader closed"));
    }

    #[tokio::test]
    async fn collect_write_data_nothing_to_do_when_data_already_complete() {
        // cmd.final_bit set and data already covers EDTL → both phases
        // are no-ops, returns Ok immediately.
        let (mgr, tsih, cid) = session_with_connection();
        let mut cmd = build_empty_pdu(0x01, false, true);
        cmd.itt = 0x9000;
        cmd.data = vec![1, 2, 3, 4];
        let (_tx, mut rx) = tokio::sync::mpsc::channel::<Pdu>(4);
        let mut sink = Vec::new();
        collect_write_data(&mut sink, &mut cmd, 4, &mgr, tsih, cid, &mut rx)
            .await
            .unwrap();
        assert_eq!(cmd.data, [1, 2, 3, 4]);
        assert!(sink.is_empty(), "no R2T needed");
    }

    // ===== End-to-end: handle_login_phase / serve_connection / run =====

    /// Test handler: records session-close calls and answers a fixed
    /// INQUIRY-shaped Data-In so the FFP path exercises the Data-In
    /// branch.
    struct TestHandler {
        iqn: String,
    }

    #[async_trait::async_trait]
    impl ScsiHandler for TestHandler {
        fn target_iqn(&self) -> &str {
            &self.iqn
        }
        async fn dispatch(&self, req: ScsiRequest<'_>) -> ScsiResponse {
            // Opcode 0x12 (INQUIRY) → Data-In; everything else → GOOD
            // with no data so the SCSI-Response-only branch runs.
            if req.cdb[0] == 0x12 {
                ScsiResponse::good(vec![0xAA; 8])
            } else if req.cdb[0] == 0xFF {
                // Force a CHECK CONDITION with no sense → default sense
                // path in the transport.
                ScsiResponse {
                    status: ScsiStatus::CheckCondition,
                    sense: None,
                    data_in: Vec::new(),
                }
            } else {
                ScsiResponse::good(Vec::new())
            }
        }
    }

    /// Build a Login Request PDU. `csg`/`nsg`/`transit` go into BHS[1].
    fn make_login_pdu(csg: u8, nsg: u8, transit: bool, cmdsn: u32, kv: &[(&str, &str)]) -> Pdu {
        let mut p = build_empty_pdu(0x03, true, false);
        p.bhs[1] = ((csg & 0x3) << 2) | (nsg & 0x3);
        if transit {
            p.bhs[1] |= 0x80;
        }
        p.bhs[8..14].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]); // ISID
        p.bhs[24..28].copy_from_slice(&cmdsn.to_be_bytes());
        let mut data = Vec::new();
        for (k, v) in kv {
            push_kv(&mut data, k, v);
        }
        p.data = data;
        p
    }

    /// Build a SCSI Command PDU with the given single-byte opcode CDB.
    /// EDTL lives at BHS bytes 20..24 — the same offset `write_pdu`
    /// stamps from `Pdu::ttt`, so we route the EDTL through `ttt`.
    fn make_scsi_cmd(itt: u32, cmdsn: u32, lun: u8, cdb0: u8, edtl: u32) -> Pdu {
        let mut p = build_empty_pdu(0x01, false, true);
        p.itt = itt;
        p.ttt = edtl; // write_pdu stamps ttt into BHS[20..24] = EDTL
        p.bhs[1] = 0x80; // final
        p.bhs[8..16].copy_from_slice(&[0, lun, 0, 0, 0, 0, 0, 0]);
        p.bhs[24..28].copy_from_slice(&cmdsn.to_be_bytes());
        p.bhs[32] = cdb0;
        p
    }

    #[tokio::test]
    async fn login_phase_no_auth_reaches_full_feature_phase() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgr = Arc::new(SessionManager::new());
        let mgr_srv = Arc::clone(&mgr);
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            handle_login_phase(
                &mut sock,
                "iqn.test:tgt",
                mgr_srv,
                None,
                &NoopLoginAudit,
                "127.0.0.1:1",
                1,
            )
            .await
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        // Single-PDU login: CSG=security, NSG=full, transit set.
        let mut login = make_login_pdu(
            STAGE_SECURITY,
            STAGE_FULL,
            true,
            0,
            &[
                ("InitiatorName", "iqn.init:host"),
                ("SessionType", "Normal"),
            ],
        );
        write_pdu(&mut client, &mut login).await.unwrap();
        let resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(resp.opcode, 0x23, "Login Response");
        // Transit bit set → entering FFP, TSIH non-zero in BHS[14..16].
        let tsih = u16::from_be_bytes([resp.bhs[14], resp.bhs[15]]);
        assert_ne!(tsih, 0, "operational session got a real TSIH");

        let outcome = server.await.unwrap().unwrap();
        assert!(!outcome.is_discovery);
        assert_ne!(outcome.tsih, 0);
        assert_eq!(outcome.initiator_iqn.as_deref(), Some("iqn.init:host"));
        assert!(mgr.session_exists(outcome.tsih));
    }

    #[tokio::test]
    async fn login_phase_discovery_session_keeps_tsih_zero() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgr = Arc::new(SessionManager::new());
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            handle_login_phase(
                &mut sock,
                "iqn.test:tgt",
                mgr,
                None,
                &NoopLoginAudit,
                "127.0.0.1:2",
                1,
            )
            .await
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut login = make_login_pdu(
            STAGE_SECURITY,
            STAGE_FULL,
            true,
            0,
            &[
                ("InitiatorName", "iqn.init:disc"),
                ("SessionType", "Discovery"),
            ],
        );
        write_pdu(&mut client, &mut login).await.unwrap();
        let _ = read_pdu(&mut client).await.unwrap();
        let outcome = server.await.unwrap().unwrap();
        assert!(outcome.is_discovery);
        assert_eq!(outcome.tsih, 0, "discovery session uses TSIH 0");
    }

    #[tokio::test]
    async fn login_phase_rejects_non_login_opcode() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgr = Arc::new(SessionManager::new());
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            handle_login_phase(
                &mut sock,
                "iqn.test:tgt",
                mgr,
                None,
                &NoopLoginAudit,
                "127.0.0.1:3",
                1,
            )
            .await
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        // A NOP-Out (0x00) where a Login Request is expected.
        let mut bad = build_empty_pdu(0x00, false, false);
        write_pdu(&mut client, &mut bad).await.unwrap();
        let err = server.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("expected Login Request"));
    }

    #[tokio::test]
    async fn login_phase_auth_required_rejects_skipped_security_stage() {
        use crate::auth::{ChapAlgorithm, ChapUser};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgr = Arc::new(SessionManager::new());
        let factory: ChapAuthFactory = Arc::new(|| {
            Ok(ChapAuthenticator::new(
                vec![ChapUser::new("u".into(), "pw".into(), false)],
                None,
                None,
                vec![ChapAlgorithm::Sha256],
            ))
        });
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            handle_login_phase(
                &mut sock,
                "iqn.test:tgt",
                mgr,
                Some(&factory),
                &NoopLoginAudit,
                "127.0.0.1:4",
                1,
            )
            .await
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        // Jump straight to opneg (CSG=1) without the security stage.
        let mut login = make_login_pdu(
            STAGE_OPNEG,
            STAGE_FULL,
            true,
            0,
            &[("InitiatorName", "iqn.init:host")],
        );
        write_pdu(&mut client, &mut login).await.unwrap();
        // Target sends a rejection Login Response then errors out.
        let resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(resp.bhs[36], 0x02, "authentication failure class");
        let err = server.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("Authentication required"));
    }

    #[tokio::test]
    async fn login_phase_chap_config_load_failure_is_rejected() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgr = Arc::new(SessionManager::new());
        let factory: ChapAuthFactory = Arc::new(|| Err(anyhow!("iscsi-users.json unreadable")));
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            handle_login_phase(
                &mut sock,
                "iqn.test:tgt",
                mgr,
                Some(&factory),
                &NoopLoginAudit,
                "127.0.0.1:5",
                1,
            )
            .await
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut login = make_login_pdu(STAGE_SECURITY, STAGE_FULL, true, 0, &[]);
        write_pdu(&mut client, &mut login).await.unwrap();
        let resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(resp.bhs[36], 0x02, "rejection: authentication failure");
        let err = server.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("CHAP config load failed"));
    }

    #[tokio::test]
    async fn serve_connection_handles_nop_scsi_and_logout() {
        // Full end-to-end: login (no auth), NOP-Out ping, INQUIRY
        // (Data-In branch), TEST UNIT READY (Response-only branch),
        // CHECK CONDITION with default sense, then Logout.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgr = Arc::new(SessionManager::new());
        let handler = Arc::new(TestHandler {
            iqn: "iqn.test:tgt".into(),
        });
        let mgr_srv = Arc::clone(&mgr);
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            serve_connection(
                sock,
                handler,
                mgr_srv,
                None,
                &NoopLoginAudit,
                "127.0.0.1:6",
                &[],
                1,
            )
            .await
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut login = make_login_pdu(
            STAGE_SECURITY,
            STAGE_FULL,
            true,
            0,
            &[("InitiatorName", "iqn.init:e2e")],
        );
        write_pdu(&mut client, &mut login).await.unwrap();
        let login_resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(login_resp.opcode, 0x23);

        // NOP-Out ping (immediate) → NOP-In.
        let mut nop = build_empty_pdu(0x00, true, true);
        nop.itt = 0x0101;
        write_pdu(&mut client, &mut nop).await.unwrap();
        let nop_in = read_pdu(&mut client).await.unwrap();
        assert_eq!(nop_in.opcode, 0x20, "NOP-In");
        assert_eq!(nop_in.itt, 0x0101);
        assert_eq!(nop_in.ttt, 0xFFFF_FFFF);

        // SendTargets Text Request.
        let mut text = build_empty_pdu(0x04, true, true);
        text.itt = 0x0202;
        push_kv(&mut text.data, "SendTargets", "All");
        write_pdu(&mut client, &mut text).await.unwrap();
        let text_resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(text_resp.opcode, 0x24, "Text Response");
        let kv = parse_text_kv(&text_resp.data);
        assert_eq!(kv.get("TargetName"), Some(&"iqn.test:tgt".to_string()));

        // INQUIRY (opcode 0x12) → Data-In PDU.
        let mut inq = make_scsi_cmd(0x0303, 1, 0, 0x12, 8);
        write_pdu(&mut client, &mut inq).await.unwrap();
        let din = read_pdu(&mut client).await.unwrap();
        assert_eq!(din.opcode, 0x25, "Data-In");
        assert_eq!(din.data, vec![0xAA; 8]);

        // TEST UNIT READY (opcode 0x00 CDB) → SCSI Response, no data.
        let mut tur = make_scsi_cmd(0x0404, 2, 0, 0x00, 0);
        write_pdu(&mut client, &mut tur).await.unwrap();
        let tur_resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(tur_resp.opcode, 0x21, "SCSI Response");
        assert_eq!(tur_resp.bhs[3], 0x00, "GOOD status");

        // CHECK CONDITION path with no sense → transport default sense.
        let mut cc = make_scsi_cmd(0x0505, 3, 0, 0xFF, 0);
        write_pdu(&mut client, &mut cc).await.unwrap();
        let cc_resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(cc_resp.opcode, 0x21);
        assert_eq!(cc_resp.bhs[3], 0x02, "CHECK CONDITION status");
        assert!(!cc_resp.data.is_empty(), "default sense payload present");

        // Task Management request → TMF Response.
        let mut tmf = build_empty_pdu(0x02, false, true);
        tmf.itt = 0x0606;
        tmf.bhs[24..28].copy_from_slice(&4u32.to_be_bytes());
        write_pdu(&mut client, &mut tmf).await.unwrap();
        let tmf_resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(tmf_resp.opcode, 0x22, "Task Mgmt Response");

        // Logout → Logout Response, then the loop breaks.
        let mut logout = build_empty_pdu(0x06, false, true);
        logout.itt = 0x0707;
        logout.bhs[24..28].copy_from_slice(&5u32.to_be_bytes());
        write_pdu(&mut client, &mut logout).await.unwrap();
        let logout_resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(logout_resp.opcode, 0x26, "Logout Response");

        server.await.unwrap().unwrap();
        // Session was cleaned up by the SessionGuard on exit.
        assert_eq!(mgr.session_count(), 0);
    }

    #[tokio::test]
    async fn serve_connection_unsupported_opcode_emits_reject() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgr = Arc::new(SessionManager::new());
        let handler = Arc::new(TestHandler {
            iqn: "iqn.test:tgt".into(),
        });
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            serve_connection(
                sock,
                handler,
                mgr,
                None,
                &NoopLoginAudit,
                "127.0.0.1:7",
                &[],
                1,
            )
            .await
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut login = make_login_pdu(STAGE_SECURITY, STAGE_FULL, true, 0, &[]);
        write_pdu(&mut client, &mut login).await.unwrap();
        let _ = read_pdu(&mut client).await.unwrap();

        // Opcode 0x1E (PREVENT/ALLOW-ish, unsupported by the transport
        // dispatcher) → Reject PDU.
        let mut bad = build_empty_pdu(0x1E, false, true);
        bad.itt = 0x0808;
        bad.bhs[24..28].copy_from_slice(&1u32.to_be_bytes());
        write_pdu(&mut client, &mut bad).await.unwrap();
        let rej = read_pdu(&mut client).await.unwrap();
        assert_eq!(rej.opcode, 0x3F, "Reject PDU");
        assert_eq!(rej.bhs[2], 0x09, "command not supported reason");

        // Close the connection so serve_connection returns.
        drop(client);
        let _ = server.await.unwrap();
    }

    #[tokio::test]
    async fn serve_connection_stray_data_out_emits_reject() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgr = Arc::new(SessionManager::new());
        let handler = Arc::new(TestHandler {
            iqn: "iqn.test:tgt".into(),
        });
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            serve_connection(
                sock,
                handler,
                mgr,
                None,
                &NoopLoginAudit,
                "127.0.0.1:8",
                &[],
                1,
            )
            .await
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut login = make_login_pdu(STAGE_SECURITY, STAGE_FULL, true, 0, &[]);
        write_pdu(&mut client, &mut login).await.unwrap();
        let _ = read_pdu(&mut client).await.unwrap();

        // Data-Out (0x05) with no matching Cmd route → stray, Reject.
        let mut stray = build_empty_pdu(0x05, false, true);
        stray.itt = 0xDEAD;
        stray.ttt = 0xFFFF_FFFF;
        stray.bhs[24..28].copy_from_slice(&1u32.to_be_bytes());
        write_pdu(&mut client, &mut stray).await.unwrap();
        let rej = read_pdu(&mut client).await.unwrap();
        assert_eq!(rej.opcode, 0x3F, "Reject PDU");
        assert_eq!(rej.bhs[2], 0x04, "protocol error reason");
        drop(client);
        let _ = server.await.unwrap();
    }

    // Regression for issue #41: the discovery-session FFP loop must
    // answer NOP-Out and Logout with the per-connection statsn counter
    // instead of routing through SessionManager (TSIH=0 has no entry).
    // Before the fix, `iscsi-ls iscsi://127.0.0.1:<port>` hung on Logout
    // because the daemon returned IscsiError::InvalidSession(0) and
    // dropped the connection without writing a Logout Response.
    #[tokio::test]
    async fn serve_connection_discovery_session_completes_logout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgr = Arc::new(SessionManager::new());
        let handler = Arc::new(TestHandler {
            iqn: "iqn.test:tgt".into(),
        });
        let mgr_srv = Arc::clone(&mgr);
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            serve_connection(
                sock,
                handler,
                mgr_srv,
                None,
                &NoopLoginAudit,
                "127.0.0.1:9",
                &[],
                1,
            )
            .await
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut login = make_login_pdu(
            STAGE_SECURITY,
            STAGE_FULL,
            true,
            0,
            &[
                ("InitiatorName", "iqn.init:disc-e2e"),
                ("SessionType", "Discovery"),
            ],
        );
        write_pdu(&mut client, &mut login).await.unwrap();
        let login_resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(login_resp.opcode, 0x23, "Login Response");
        // TSIH must be 0 — this is the precondition the bug fix covers.
        let resp_tsih = u16::from_be_bytes([login_resp.bhs[14], login_resp.bhs[15]]);
        assert_eq!(resp_tsih, 0, "discovery session must keep TSIH=0");

        // NOP-Out → NOP-In (was the second failure path).
        let mut nop = build_empty_pdu(0x00, true, true);
        nop.itt = 0x0111;
        write_pdu(&mut client, &mut nop).await.unwrap();
        let nop_in = read_pdu(&mut client).await.unwrap();
        assert_eq!(nop_in.opcode, 0x20, "NOP-In");
        assert_eq!(nop_in.itt, 0x0111);

        // SendTargets Text Request → Text Response naming the target.
        let mut text = build_empty_pdu(0x04, true, true);
        text.itt = 0x0222;
        push_kv(&mut text.data, "SendTargets", "All");
        write_pdu(&mut client, &mut text).await.unwrap();
        let text_resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(text_resp.opcode, 0x24, "Text Response");
        let kv = parse_text_kv(&text_resp.data);
        assert_eq!(kv.get("TargetName"), Some(&"iqn.test:tgt".to_string()));

        // Logout → Logout Response (the originally-broken path).
        let mut logout = build_empty_pdu(0x06, false, true);
        logout.itt = 0x0333;
        logout.bhs[24..28].copy_from_slice(&1u32.to_be_bytes());
        write_pdu(&mut client, &mut logout).await.unwrap();
        let logout_resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(logout_resp.opcode, 0x26, "Logout Response");
        assert_eq!(logout_resp.bhs[2], 0x00, "logout success");

        // serve_connection should return Ok(()) — not the pre-fix
        // anyhow-wrapped IscsiError::InvalidSession(0).
        server.await.unwrap().unwrap();
        // Discovery session was never registered in SessionManager.
        assert_eq!(mgr.session_count(), 0);
    }

    // Discovery sessions are restricted to Login/Logout/Text/NOP-Out
    // by RFC 7143 §6.1 — SCSI Command and TMF must be rejected with a
    // typed protocol-error Reject rather than dispatched into the
    // handler (the pre-fix path returned an opaque InvalidSession(0)
    // and closed the socket).
    #[tokio::test]
    async fn serve_connection_discovery_session_rejects_scsi_and_tmf() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgr = Arc::new(SessionManager::new());
        let handler = Arc::new(TestHandler {
            iqn: "iqn.test:tgt".into(),
        });
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            serve_connection(
                sock,
                handler,
                mgr,
                None,
                &NoopLoginAudit,
                "127.0.0.1:10",
                &[],
                1,
            )
            .await
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut login = make_login_pdu(
            STAGE_SECURITY,
            STAGE_FULL,
            true,
            0,
            &[
                ("InitiatorName", "iqn.init:disc-illegal"),
                ("SessionType", "Discovery"),
            ],
        );
        write_pdu(&mut client, &mut login).await.unwrap();
        let _ = read_pdu(&mut client).await.unwrap();

        let mut scsi = make_scsi_cmd(0x0101, 1, 0, 0x12, 8);
        write_pdu(&mut client, &mut scsi).await.unwrap();
        let rej = read_pdu(&mut client).await.unwrap();
        assert_eq!(rej.opcode, 0x3F, "Reject PDU for SCSI Cmd in discovery");
        assert_eq!(rej.bhs[2], 0x04, "protocol error reason");

        let mut tmf = build_empty_pdu(0x02, false, true);
        tmf.itt = 0x0202;
        tmf.bhs[24..28].copy_from_slice(&2u32.to_be_bytes());
        write_pdu(&mut client, &mut tmf).await.unwrap();
        let rej2 = read_pdu(&mut client).await.unwrap();
        assert_eq!(rej2.opcode, 0x3F, "Reject PDU for TMF in discovery");
        assert_eq!(rej2.bhs[2], 0x04, "protocol error reason");

        // Logout cleanly to close the session.
        let mut logout = build_empty_pdu(0x06, false, true);
        logout.itt = 0x0303;
        logout.bhs[24..28].copy_from_slice(&3u32.to_be_bytes());
        write_pdu(&mut client, &mut logout).await.unwrap();
        let logout_resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(logout_resp.opcode, 0x26);

        server.await.unwrap().unwrap();
    }

    /// Test handler that returns a deterministic N-byte payload for
    /// any SCSI command with CDB[0] = 0xC0 (vendor-unique). The
    /// length is set at construction. Used to drive multi-PDU
    /// Data-In chunking without dragging in the SBC dispatcher.
    struct BigReadHandler {
        iqn: String,
        payload_len: usize,
    }

    #[async_trait::async_trait]
    impl ScsiHandler for BigReadHandler {
        fn target_iqn(&self) -> &str {
            &self.iqn
        }
        async fn dispatch(&self, req: ScsiRequest<'_>) -> ScsiResponse {
            if req.cdb[0] == 0xC0 {
                let mut buf = Vec::with_capacity(self.payload_len);
                for i in 0..self.payload_len {
                    buf.push((i & 0xFF) as u8);
                }
                ScsiResponse::good(buf)
            } else {
                ScsiResponse::good(Vec::new())
            }
        }
    }

    #[tokio::test]
    async fn data_in_chunked_at_peer_max_recv_data_segment_length() {
        // Reproduces the multi-sector READ short-transfer / EPIPE bug:
        // the target used to send one Data-In PDU regardless of size,
        // even when the initiator's MaxRecvDataSegmentLength was
        // smaller. A 64 KiB payload against an initiator that
        // declared 8 KiB MRDSL MUST come back as 8 back-to-back
        // Data-In PDUs, each with the correct DataSN / BufferOffset,
        // F=1 + S=1 only on the last, and the concatenated payload
        // identical to what the handler returned.
        const PAYLOAD: usize = 64 * 1024;
        const PEER_MRDSL: u32 = 8192;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgr = Arc::new(SessionManager::new());
        let handler = Arc::new(BigReadHandler {
            iqn: "iqn.test:big".into(),
            payload_len: PAYLOAD,
        });
        let mgr_srv = Arc::clone(&mgr);
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            serve_connection(
                sock,
                handler,
                mgr_srv,
                None,
                &NoopLoginAudit,
                "127.0.0.1:99",
                &[],
                1,
            )
            .await
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        // Declare an 8 KiB MRDSL during the single-PDU login.
        let mut login = make_login_pdu(
            STAGE_SECURITY,
            STAGE_FULL,
            true,
            0,
            &[
                ("InitiatorName", "iqn.init:chunk-test"),
                ("MaxRecvDataSegmentLength", "8192"),
            ],
        );
        write_pdu(&mut client, &mut login).await.unwrap();
        let _ = read_pdu(&mut client).await.unwrap();

        // SCSI Command with CDB[0]=0xC0 and EDTL = PAYLOAD bytes.
        let mut cmd = make_scsi_cmd(0xBEEF, 1, 0, 0xC0, PAYLOAD as u32);
        write_pdu(&mut client, &mut cmd).await.unwrap();

        // Pull every Data-In PDU off the wire and verify the chunking.
        let expected_chunks = PAYLOAD.div_ceil(PEER_MRDSL as usize);
        let mut collected: Vec<u8> = Vec::with_capacity(PAYLOAD);
        for chunk_idx in 0..expected_chunks {
            let pdu = read_pdu(&mut client).await.unwrap();
            assert_eq!(pdu.opcode, 0x25, "expected Data-In on chunk {chunk_idx}");
            let is_last = chunk_idx == expected_chunks - 1;
            let f_bit = (pdu.bhs[1] & 0x80) != 0;
            let s_bit = (pdu.bhs[1] & 0x01) != 0;
            let data_sn = u32::from_be_bytes([pdu.bhs[36], pdu.bhs[37], pdu.bhs[38], pdu.bhs[39]]);
            let buffer_offset =
                u32::from_be_bytes([pdu.bhs[40], pdu.bhs[41], pdu.bhs[42], pdu.bhs[43]]);
            assert_eq!(
                data_sn, chunk_idx as u32,
                "DataSN must be 0..N sequential, chunk {chunk_idx}"
            );
            assert_eq!(
                buffer_offset as usize,
                chunk_idx * PEER_MRDSL as usize,
                "BufferOffset must walk by chunk size, chunk {chunk_idx}"
            );
            if is_last {
                assert!(f_bit, "last Data-In must have F=1");
                assert!(s_bit, "last Data-In must have S=1 (carries status)");
                assert_eq!(pdu.bhs[3], 0x00, "GOOD status on the last Data-In");
            } else {
                assert!(!f_bit, "non-last Data-In must have F=0 (chunk {chunk_idx})");
                assert!(!s_bit, "non-last Data-In must have S=0 (chunk {chunk_idx})");
            }
            assert!(
                pdu.data.len() <= PEER_MRDSL as usize,
                "data segment {} > MRDSL {} on chunk {chunk_idx}",
                pdu.data.len(),
                PEER_MRDSL
            );
            collected.extend_from_slice(&pdu.data);
        }
        assert_eq!(collected.len(), PAYLOAD, "total bytes match payload");
        for (i, b) in collected.iter().enumerate() {
            assert_eq!(*b, (i & 0xFF) as u8, "byte {i} round-tripped intact");
        }

        // Clean logout so serve_connection returns cleanly.
        let mut logout = build_empty_pdu(0x06, false, true);
        logout.itt = 0xAA;
        logout.bhs[24..28].copy_from_slice(&2u32.to_be_bytes());
        write_pdu(&mut client, &mut logout).await.unwrap();
        let _ = read_pdu(&mut client).await.unwrap();
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn data_in_single_pdu_when_payload_fits_in_peer_mrdsl() {
        // Sub-MRDSL payloads collapse to exactly one Data-In PDU with
        // F=1 + S=1, DataSN=0, BufferOffset=0 — the historical fast
        // path. Anchors that chunking didn't regress single-PDU reads.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgr = Arc::new(SessionManager::new());
        let handler = Arc::new(BigReadHandler {
            iqn: "iqn.test:small".into(),
            payload_len: 256,
        });
        let mgr_srv = Arc::clone(&mgr);
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            serve_connection(
                sock,
                handler,
                mgr_srv,
                None,
                &NoopLoginAudit,
                "127.0.0.1:100",
                &[],
                1,
            )
            .await
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut login = make_login_pdu(
            STAGE_SECURITY,
            STAGE_FULL,
            true,
            0,
            &[("InitiatorName", "iqn.init:small")],
        );
        write_pdu(&mut client, &mut login).await.unwrap();
        let _ = read_pdu(&mut client).await.unwrap();
        let mut cmd = make_scsi_cmd(0xCAFE, 1, 0, 0xC0, 256);
        write_pdu(&mut client, &mut cmd).await.unwrap();
        let pdu = read_pdu(&mut client).await.unwrap();
        assert_eq!(pdu.opcode, 0x25);
        assert_eq!(pdu.bhs[1] & 0x80, 0x80, "F=1");
        assert_eq!(pdu.bhs[1] & 0x01, 0x01, "S=1");
        let data_sn = u32::from_be_bytes([pdu.bhs[36], pdu.bhs[37], pdu.bhs[38], pdu.bhs[39]]);
        let buf_off = u32::from_be_bytes([pdu.bhs[40], pdu.bhs[41], pdu.bhs[42], pdu.bhs[43]]);
        assert_eq!(data_sn, 0);
        assert_eq!(buf_off, 0);
        assert_eq!(pdu.data.len(), 256);
        let mut logout = build_empty_pdu(0x06, false, true);
        logout.itt = 0xBB;
        logout.bhs[24..28].copy_from_slice(&2u32.to_be_bytes());
        write_pdu(&mut client, &mut logout).await.unwrap();
        let _ = read_pdu(&mut client).await.unwrap();
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn data_in_default_peer_mrdsl_chunks_at_8192_when_initiator_omits_key() {
        // Initiator omits MaxRecvDataSegmentLength → RFC 7143 §12.12
        // default of 8192. A 24 KiB payload arrives as 3 PDUs.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgr = Arc::new(SessionManager::new());
        let handler = Arc::new(BigReadHandler {
            iqn: "iqn.test:default".into(),
            payload_len: 24 * 1024,
        });
        let mgr_srv = Arc::clone(&mgr);
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            serve_connection(
                sock,
                handler,
                mgr_srv,
                None,
                &NoopLoginAudit,
                "127.0.0.1:101",
                &[],
                1,
            )
            .await
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        // No MaxRecvDataSegmentLength key.
        let mut login = make_login_pdu(
            STAGE_SECURITY,
            STAGE_FULL,
            true,
            0,
            &[("InitiatorName", "iqn.init:no-mrdsl")],
        );
        write_pdu(&mut client, &mut login).await.unwrap();
        let _ = read_pdu(&mut client).await.unwrap();
        let mut cmd = make_scsi_cmd(0xC0FFEE, 1, 0, 0xC0, 24 * 1024);
        write_pdu(&mut client, &mut cmd).await.unwrap();
        for chunk in 0..3 {
            let pdu = read_pdu(&mut client).await.unwrap();
            assert_eq!(pdu.opcode, 0x25);
            assert_eq!(
                pdu.data.len(),
                8192,
                "chunk {chunk}: 8192 (RFC default MRDSL)"
            );
            if chunk == 2 {
                assert_eq!(pdu.bhs[1] & 0x81, 0x81, "last Data-In F|S");
            } else {
                assert_eq!(pdu.bhs[1] & 0x81, 0, "non-last Data-In no F|S");
            }
        }
        let mut logout = build_empty_pdu(0x06, false, true);
        logout.itt = 0xCC;
        logout.bhs[24..28].copy_from_slice(&2u32.to_be_bytes());
        write_pdu(&mut client, &mut logout).await.unwrap();
        let _ = read_pdu(&mut client).await.unwrap();
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn run_binds_and_serves_a_connection() {
        // Exercise `run`: bind, accept, serve one connection through
        // login + logout, then the task is aborted.
        let mgr = Arc::new(SessionManager::new());
        let handler = Arc::new(TestHandler {
            iqn: "iqn.test:run".into(),
        });
        let config = ServerConfig {
            listen_portals: vec![Portal {
                address: "127.0.0.1:0".to_string(),
                tpgt: 1,
            }],
            session_manager: Arc::clone(&mgr),
            auth: None,
            audit: Arc::new(NoopLoginAudit),
            stale_session_timeout_secs: 300,
        };
        // `run` takes listen_portals by value and binds them; bind to
        // port 0 then discover the port is not possible through `run`
        // directly, so bind a probe listener, take its port, drop it,
        // and reuse — small race but fine for a unit test.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let config = ServerConfig {
            listen_portals: vec![Portal {
                address: format!("127.0.0.1:{port}"),
                tpgt: 1,
            }],
            ..config
        };
        let server = tokio::spawn(run(config, handler));

        // Give the listener a moment to bind, then connect.
        let mut client = None;
        for _ in 0..50 {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(c) => {
                    client = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
        let mut client = client.expect("connect to run() listener");
        let mut login = make_login_pdu(STAGE_SECURITY, STAGE_FULL, true, 0, &[]);
        write_pdu(&mut client, &mut login).await.unwrap();
        let resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(resp.opcode, 0x23, "run() served the login");

        let mut logout = build_empty_pdu(0x06, false, true);
        logout.itt = 0x01;
        logout.bhs[24..28].copy_from_slice(&1u32.to_be_bytes());
        write_pdu(&mut client, &mut logout).await.unwrap();
        let logout_resp = read_pdu(&mut client).await.unwrap();
        assert_eq!(logout_resp.opcode, 0x26);

        server.abort();
    }

    #[test]
    fn server_config_carries_transport_hooks() {
        let config = ServerConfig {
            listen_portals: vec![Portal {
                address: "0.0.0.0:3260".to_string(),
                tpgt: 1,
            }],
            session_manager: Arc::new(SessionManager::new()),
            auth: None,
            audit: Arc::new(NoopLoginAudit),
            stale_session_timeout_secs: 600,
        };
        assert_eq!(
            config.listen_portals,
            vec![Portal {
                address: "0.0.0.0:3260".to_string(),
                tpgt: 1
            }]
        );
        assert_eq!(config.stale_session_timeout_secs, 600);
        assert!(config.auth.is_none());
    }

    fn portal(addr: &str, tpgt: u16) -> Portal {
        Portal {
            address: addr.to_string(),
            tpgt,
        }
    }

    #[test]
    fn build_target_addresses_emits_one_line_per_concrete_portal() {
        let advertised = vec![portal("10.0.0.5:3260", 1), portal("10.0.0.6:3260", 1)];
        let local: SocketAddr = "10.0.0.5:3260".parse().unwrap();
        let out = build_target_addresses(&advertised, local);
        assert_eq!(
            out,
            vec![
                ("10.0.0.5:3260".to_string(), 1),
                ("10.0.0.6:3260".to_string(), 1)
            ]
        );
    }

    #[test]
    fn build_target_addresses_carries_per_portal_tpgt() {
        // Distinct TPGTs flow through unchanged — the SendTargets
        // emitter renders each as `TargetAddress=<addr>,<tpgt>` and
        // initiators use the tag to model per-path ALUA state.
        let advertised = vec![portal("10.0.0.5:3260", 1), portal("10.0.0.6:3260", 2)];
        let local: SocketAddr = "10.0.0.5:3260".parse().unwrap();
        let out = build_target_addresses(&advertised, local);
        assert_eq!(
            out,
            vec![
                ("10.0.0.5:3260".to_string(), 1),
                ("10.0.0.6:3260".to_string(), 2)
            ]
        );
    }

    #[test]
    fn build_target_addresses_substitutes_wildcard_with_local_ip() {
        // Legacy single-portal happy path: configured 0.0.0.0:3260,
        // connection landed on 192.0.2.7:3260 -> emit the concrete
        // local address (not the wildcard).
        let advertised = vec![portal("0.0.0.0:3260", 1)];
        let local: SocketAddr = "192.0.2.7:3260".parse().unwrap();
        let out = build_target_addresses(&advertised, local);
        assert_eq!(out, vec![("192.0.2.7:3260".to_string(), 1)]);
    }

    #[test]
    fn build_target_addresses_substitutes_wildcard_preserving_each_port() {
        // Wildcard substitution keeps the *configured* port — emitting
        // local.port() would collapse multi-port wildcard binds into
        // one line. Mixed wildcard + literal also keeps the literal
        // intact.
        let advertised = vec![
            portal("0.0.0.0:3260", 1),
            portal("0.0.0.0:3261", 2),
            portal("192.0.2.9:3260", 3),
        ];
        let local: SocketAddr = "192.0.2.7:3260".parse().unwrap();
        let out = build_target_addresses(&advertised, local);
        assert_eq!(
            out,
            vec![
                ("192.0.2.7:3260".to_string(), 1),
                ("192.0.2.7:3261".to_string(), 2),
                ("192.0.2.9:3260".to_string(), 3),
            ]
        );
    }

    #[test]
    fn build_target_addresses_dedupes_after_substitution() {
        // Wildcard 0.0.0.0:3260 + literal 192.0.2.7:3260 collapse to
        // the same line after substitution — the initiator should not
        // see the same record twice. The first occurrence's TPGT
        // wins; emitting two TargetAddress lines with the same
        // ip:port and different TPGTs would confuse the initiator.
        let advertised = vec![portal("0.0.0.0:3260", 1), portal("192.0.2.7:3260", 2)];
        let local: SocketAddr = "192.0.2.7:3260".parse().unwrap();
        let out = build_target_addresses(&advertised, local);
        assert_eq!(out, vec![("192.0.2.7:3260".to_string(), 1)]);
    }

    #[test]
    fn build_target_addresses_handles_ipv6_wildcard_with_brackets() {
        // SocketAddr Display brackets IPv6 — `[fe80::1]:3260`, not
        // `fe80::1:3260` (which would be ambiguous with port-in-low-
        // word forms).
        let advertised = vec![portal("[::]:3260", 1)];
        let local: SocketAddr = "[fe80::1]:3260".parse().unwrap();
        let out = build_target_addresses(&advertised, local);
        assert_eq!(out, vec![("[fe80::1]:3260".to_string(), 1)]);
    }

    #[tokio::test]
    async fn run_rejects_duplicate_listen_addresses() {
        // Two entries with the same `ip:port` (regardless of TPGT)
        // would fail at bind(2) anyway and would hand the initiator
        // two TargetAddress lines for the same socket — `run` catches
        // it before either listener is created.
        let config = ServerConfig {
            listen_portals: vec![
                Portal {
                    address: "127.0.0.1:65535".to_string(),
                    tpgt: 1,
                },
                Portal {
                    address: "127.0.0.1:65535".to_string(),
                    tpgt: 2,
                },
            ],
            session_manager: Arc::new(SessionManager::new()),
            auth: None,
            audit: Arc::new(NoopLoginAudit),
            stale_session_timeout_secs: 300,
        };
        let handler = Arc::new(TestHandler {
            iqn: "iqn.test:dup".into(),
        });
        let err = run(config, handler).await.unwrap_err();
        assert!(
            err.to_string().contains("duplicate listen address"),
            "want 'duplicate listen address' in error, got: {err}"
        );
    }

    #[tokio::test]
    async fn login_response_carries_per_portal_tpgt() {
        // The arrival portal's TPGT flows into the Login Response
        // `TargetPortalGroupTag` key (RFC 7143 §12.10).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgr = Arc::new(SessionManager::new());
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            handle_login_phase(
                &mut sock,
                "iqn.test:tgt",
                mgr,
                None,
                &NoopLoginAudit,
                "127.0.0.1:tpgt",
                7,
            )
            .await
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut login = make_login_pdu(
            STAGE_SECURITY,
            STAGE_FULL,
            true,
            0,
            &[("InitiatorName", "iqn.init:tpgt")],
        );
        write_pdu(&mut client, &mut login).await.unwrap();
        let resp = read_pdu(&mut client).await.unwrap();
        let kv = parse_text_kv(&resp.data);
        assert_eq!(
            kv.get("TargetPortalGroupTag"),
            Some(&"7".to_string()),
            "Login Response echoes arrival portal's TPGT"
        );
        let _ = server.await.unwrap();
    }

    #[tokio::test]
    async fn sendtargets_renders_per_portal_tpgt() {
        // SendTargets text response carries `TargetAddress=ip:port,tpgt`
        // per portal, using each portal's own TPGT.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mgr = Arc::new(SessionManager::new());
        let handler = Arc::new(TestHandler {
            iqn: "iqn.test:tgt".into(),
        });
        let advertised = vec![portal("10.0.0.5:3260", 1), portal("10.0.0.6:3260", 2)];
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            serve_connection(
                sock,
                handler,
                mgr,
                None,
                &NoopLoginAudit,
                "127.0.0.1:st",
                &advertised,
                1,
            )
            .await
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut login = make_login_pdu(
            STAGE_SECURITY,
            STAGE_FULL,
            true,
            0,
            &[
                ("InitiatorName", "iqn.init:st"),
                ("SessionType", "Discovery"),
            ],
        );
        write_pdu(&mut client, &mut login).await.unwrap();
        let _ = read_pdu(&mut client).await.unwrap();

        let mut text = build_empty_pdu(0x04, true, true);
        text.itt = 0x4242;
        push_kv(&mut text.data, "SendTargets", "All");
        write_pdu(&mut client, &mut text).await.unwrap();
        let text_resp = read_pdu(&mut client).await.unwrap();
        let text_data = String::from_utf8_lossy(&text_resp.data);
        assert!(
            text_data.contains("TargetAddress=10.0.0.5:3260,1"),
            "missing portal 1 line: {text_data}"
        );
        assert!(
            text_data.contains("TargetAddress=10.0.0.6:3260,2"),
            "missing portal 2 line: {text_data}"
        );

        drop(client);
        let _ = server.await.unwrap();
    }
}
