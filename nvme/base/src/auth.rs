// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVMe in-band authentication wire shapes (NVMe Base §8.13, DH-HMAC-CHAP).
//!
//! NVMe-oF carries host authentication over two Fabrics commands —
//! Authentication Send (FCTYPE 0x05, host -> controller) and
//! Authentication Receive (FCTYPE 0x06, controller -> host) — whose
//! data payloads are the DH-HMAC-CHAP protocol messages. The exchange
//! is strictly serialized once the controller asserts AUTHREQ in the
//! Connect Response:
//!
//! ```text
//!   host -> Negotiate    (Auth Send)     offered hashes + DH groups
//!   ctrl -> Challenge    (Auth Receive)  picked hash/group, C1, S1, g^x
//!   host -> Reply        (Auth Send)     R1, optional C2, S2, g^y
//!   ctrl -> Success1     (Auth Receive)  optional R2 (mutual auth)
//!   host -> Success2     (Auth Send)     acknowledgement
//! ```
//!
//! This module is the pure wire layer: message (de)serializers and the
//! Auth Send/Receive command-dword decode. No crypto and no I/O — the
//! HMAC / Diffie-Hellman computation and the controller-side state
//! machine live in `nvme-tcp` (`crate::auth` / `crate::server`). The
//! byte layouts are taken verbatim from the Linux kernel headers
//! (`include/linux/nvme.h`, the `nvmf_auth_dhchap_*` structs) so a
//! stock `nvme connect --dhchap-secret` host interoperates.
//!
//! Two `auth_type` namespaces appear on the wire: protocol-agnostic
//! messages (Negotiate, Failure) carry [`NVME_AUTH_COMMON_MESSAGES`],
//! DH-HMAC-CHAP-specific ones (Challenge, Reply, Success1, Success2)
//! carry [`NVME_AUTH_DHCHAP_MESSAGES`]. We validate the pair on parse.

use crate::sqe::Sqe;

// ===================== Constants (NVMe Base §8.13) =====================

/// Security Protocol value for DH-HMAC-CHAP in the Authentication
/// Send / Receive command's SECP field (SQE byte 43). The host sets
/// it; we validate it on the Negotiate.
pub const NVME_AUTH_DHCHAP_PROTOCOL_IDENTIFIER: u8 = 0xE9;

// `auth_type` (message byte 0).
pub const NVME_AUTH_COMMON_MESSAGES: u8 = 0x00;
pub const NVME_AUTH_DHCHAP_MESSAGES: u8 = 0x01;

// `auth_id` (message byte 1) — the message type.
pub const NVME_AUTH_DHCHAP_MESSAGE_NEGOTIATE: u8 = 0x00;
pub const NVME_AUTH_DHCHAP_MESSAGE_CHALLENGE: u8 = 0x01;
pub const NVME_AUTH_DHCHAP_MESSAGE_REPLY: u8 = 0x02;
pub const NVME_AUTH_DHCHAP_MESSAGE_SUCCESS1: u8 = 0x03;
pub const NVME_AUTH_DHCHAP_MESSAGE_SUCCESS2: u8 = 0x04;
pub const NVME_AUTH_DHCHAP_MESSAGE_FAILURE2: u8 = 0xF0;
pub const NVME_AUTH_DHCHAP_MESSAGE_FAILURE1: u8 = 0xF1;

/// `authid` inside a Negotiate protocol descriptor — selects the
/// DH-HMAC-CHAP authentication protocol (the only one we model).
pub const NVME_AUTH_DHCHAP_AUTH_ID: u8 = 0x01;

// Hash function ids (Challenge.hashid / negotiate idlist entries).
pub const NVME_AUTH_HASH_SHA256: u8 = 0x01;
pub const NVME_AUTH_HASH_SHA384: u8 = 0x02;
pub const NVME_AUTH_HASH_SHA512: u8 = 0x03;
pub const NVME_AUTH_HASH_INVALID: u8 = 0xFF;

/// Largest hash digest we handle (SHA-512).
pub const NVME_AUTH_MAX_DIGEST_SIZE: usize = 64;

// Diffie-Hellman group ids (Challenge.dhgid / negotiate idlist entries).
pub const NVME_AUTH_DHGROUP_NULL: u8 = 0x00;
pub const NVME_AUTH_DHGROUP_2048: u8 = 0x01;
pub const NVME_AUTH_DHGROUP_3072: u8 = 0x02;
pub const NVME_AUTH_DHGROUP_4096: u8 = 0x03;
pub const NVME_AUTH_DHGROUP_6144: u8 = 0x04;
pub const NVME_AUTH_DHGROUP_8192: u8 = 0x05;
pub const NVME_AUTH_DHGROUP_INVALID: u8 = 0xFF;

/// Reply.cvalid / Success1.rvalid bit 0 — "this message carries a
/// challenge/response for the peer" (the bidirectional-auth marker).
pub const NVME_AUTH_DHCHAP_RESPONSE_VALID: u8 = 1 << 0;

// Failure `rescode` — only one reason code is defined.
pub const NVME_AUTH_DHCHAP_FAILURE_REASON_FAILED: u8 = 0x01;

// Failure `rescode_exp` — the explanation byte.
pub const NVME_AUTH_DHCHAP_FAILURE_FAILED: u8 = 0x01;
pub const NVME_AUTH_DHCHAP_FAILURE_NOT_USABLE: u8 = 0x02;
pub const NVME_AUTH_DHCHAP_FAILURE_CONCAT_MISMATCH: u8 = 0x03;
pub const NVME_AUTH_DHCHAP_FAILURE_HASH_UNUSABLE: u8 = 0x04;
pub const NVME_AUTH_DHCHAP_FAILURE_DHGROUP_UNUSABLE: u8 = 0x05;
pub const NVME_AUTH_DHCHAP_FAILURE_INCORRECT_PAYLOAD: u8 = 0x06;
pub const NVME_AUTH_DHCHAP_FAILURE_INCORRECT_MESSAGE: u8 = 0x07;

/// Each Negotiate protocol descriptor is a fixed 64-byte record.
const PROTOCOL_DESCRIPTOR_LEN: usize = 64;
/// Fixed-header length shared by Negotiate / Failure (the messages
/// whose payload is byte-counted from offset 8 / has no payload).
const COMMON_HEADER_LEN: usize = 8;
/// Fixed-header length of Challenge / Reply / Success1 (variable data
/// trails it).
const DHCHAP_HEADER_LEN: usize = 16;

/// Digest length for a hash id, or `None` if unrecognized.
pub fn hash_len(hash_id: u8) -> Option<usize> {
    match hash_id {
        NVME_AUTH_HASH_SHA256 => Some(32),
        NVME_AUTH_HASH_SHA384 => Some(48),
        NVME_AUTH_HASH_SHA512 => Some(64),
        _ => None,
    }
}

/// Errors decoding an authentication message off the wire. The
/// controller maps these to an AUTH_Failure with the matching
/// `rescode_exp` (usually INCORRECT_PAYLOAD / INCORRECT_MESSAGE).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthWireError {
    #[error("authentication message too short: {got} bytes, need >= {need}")]
    TooShort { got: usize, need: usize },
    #[error("unexpected auth_type 0x{got:02X} (expected 0x{expected:02X})")]
    BadAuthType { got: u8, expected: u8 },
    #[error("unexpected auth_id 0x{got:02X} (expected 0x{expected:02X})")]
    BadMessageId { got: u8, expected: u8 },
    #[error("declared field length {got} inconsistent with message size {avail}")]
    BadLength { got: usize, avail: usize },
    #[error("hash length {got} does not match negotiated {expected}")]
    HashLenMismatch { got: usize, expected: usize },
}

// ===================== Auth Send/Receive command =====================

/// The Security Protocol / transfer-length fields the host sets on an
/// Authentication Send / Receive Fabrics command.
///
/// Fabrics SQE byte map (NVMe Base, `nvmf_auth_send_command`):
/// `... DPTR(24..40), resv3(40), spsp0(41), spsp1(42), secp(43),
/// tl/al(44..48) ...` — i.e. CDW10 packs `[resv3, spsp0, spsp1, secp]`
/// little-endian and CDW11 is the 32-bit transfer (Send) or allocation
/// (Receive) length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthCommandFields {
    pub spsp0: u8,
    pub spsp1: u8,
    pub secp: u8,
    /// TL on Authentication Send, AL on Authentication Receive.
    pub tl_al: u32,
}

/// Decode the SECP / SPSP / TL-AL fields from a Fabrics SQE that the
/// caller has already confirmed is an Authentication Send / Receive
/// (Admin opcode 0x7F, FCTYPE 0x05 / 0x06).
pub fn parse_auth_command(sqe: &Sqe) -> AuthCommandFields {
    AuthCommandFields {
        spsp0: ((sqe.cdw10 >> 8) & 0xFF) as u8,
        spsp1: ((sqe.cdw10 >> 16) & 0xFF) as u8,
        secp: ((sqe.cdw10 >> 24) & 0xFF) as u8,
        tl_al: sqe.cdw11,
    }
}

// ===================== Negotiate (host -> controller) =====================

/// One DH-HMAC-CHAP protocol descriptor from a Negotiate message: the
/// host's offered hash ids and DH group ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolDescriptor {
    pub authid: u8,
    pub hash_ids: Vec<u8>,
    pub dhgroup_ids: Vec<u8>,
}

/// Parsed Negotiate message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiateData {
    pub t_id: u16,
    /// Secure-channel concatenation byte. 0 (`NOSC`) for our
    /// deployment — fed verbatim into the response HMAC, so it must be
    /// echoed exactly. Captured here and validated by the caller.
    pub sc_c: u8,
    pub descriptors: Vec<ProtocolDescriptor>,
}

impl NegotiateData {
    /// Find the first DH-HMAC-CHAP descriptor (authid == 0x01). The
    /// kernel host sends exactly one.
    pub fn dhchap_descriptor(&self) -> Option<&ProtocolDescriptor> {
        self.descriptors
            .iter()
            .find(|d| d.authid == NVME_AUTH_DHCHAP_AUTH_ID)
    }
}

/// Parse a Negotiate message (`auth_type = COMMON`, `auth_id =
/// NEGOTIATE`). Each `napd` descriptor is a fixed 64-byte record whose
/// `idlist` carries `halen` hash ids at `[0..halen]` and `dhlen`
/// DH group ids at `[30..30+dhlen]`.
pub fn parse_negotiate(buf: &[u8]) -> Result<NegotiateData, AuthWireError> {
    if buf.len() < COMMON_HEADER_LEN {
        return Err(AuthWireError::TooShort {
            got: buf.len(),
            need: COMMON_HEADER_LEN,
        });
    }
    expect_header(
        buf,
        NVME_AUTH_COMMON_MESSAGES,
        NVME_AUTH_DHCHAP_MESSAGE_NEGOTIATE,
    )?;
    let t_id = u16::from_le_bytes([buf[4], buf[5]]);
    let sc_c = buf[6];
    let napd = buf[7] as usize;
    let need = COMMON_HEADER_LEN + napd * PROTOCOL_DESCRIPTOR_LEN;
    if buf.len() < need {
        return Err(AuthWireError::TooShort {
            got: buf.len(),
            need,
        });
    }
    let mut descriptors = Vec::with_capacity(napd);
    for i in 0..napd {
        let base = COMMON_HEADER_LEN + i * PROTOCOL_DESCRIPTOR_LEN;
        let d = &buf[base..base + PROTOCOL_DESCRIPTOR_LEN];
        let authid = d[0];
        let halen = d[2] as usize;
        let dhlen = d[3] as usize;
        // idlist is d[4..64] (60 bytes): hash ids first 30, DH ids next 30.
        if halen > 30 || dhlen > 30 {
            return Err(AuthWireError::BadLength {
                got: halen.max(dhlen),
                avail: 30,
            });
        }
        let hash_ids = d[4..4 + halen].to_vec();
        let dhgroup_ids = d[4 + 30..4 + 30 + dhlen].to_vec();
        descriptors.push(ProtocolDescriptor {
            authid,
            hash_ids,
            dhgroup_ids,
        });
    }
    Ok(NegotiateData {
        t_id,
        sc_c,
        descriptors,
    })
}

/// Encode a Negotiate message. Provided for the in-crate test harness
/// (a fake host) and round-trip tests; the production controller only
/// ever *parses* Negotiate.
pub fn build_negotiate(t_id: u16, sc_c: u8, descriptors: &[ProtocolDescriptor]) -> Vec<u8> {
    let mut out = vec![0u8; COMMON_HEADER_LEN + descriptors.len() * PROTOCOL_DESCRIPTOR_LEN];
    out[0] = NVME_AUTH_COMMON_MESSAGES;
    out[1] = NVME_AUTH_DHCHAP_MESSAGE_NEGOTIATE;
    out[4..6].copy_from_slice(&t_id.to_le_bytes());
    out[6] = sc_c;
    out[7] = descriptors.len() as u8;
    for (i, d) in descriptors.iter().enumerate() {
        let base = COMMON_HEADER_LEN + i * PROTOCOL_DESCRIPTOR_LEN;
        out[base] = d.authid;
        out[base + 2] = d.hash_ids.len() as u8;
        out[base + 3] = d.dhgroup_ids.len() as u8;
        out[base + 4..base + 4 + d.hash_ids.len()].copy_from_slice(&d.hash_ids);
        let dh_off = base + 4 + 30;
        out[dh_off..dh_off + d.dhgroup_ids.len()].copy_from_slice(&d.dhgroup_ids);
    }
    out
}

// ===================== Challenge (controller -> host) =====================

/// Encode a Challenge message (`auth_type = DHCHAP`, `auth_id =
/// CHALLENGE`): the picked hash/group, the controller challenge `cval`
/// (length = negotiated hash len), sequence number `seqnum` (S1), and
/// the controller's DH public value `dhval` (empty for the NULL group).
pub fn build_challenge(
    t_id: u16,
    hashid: u8,
    dhgid: u8,
    seqnum: u32,
    cval: &[u8],
    dhval: &[u8],
) -> Vec<u8> {
    let mut out = vec![0u8; DHCHAP_HEADER_LEN + cval.len() + dhval.len()];
    out[0] = NVME_AUTH_DHCHAP_MESSAGES;
    out[1] = NVME_AUTH_DHCHAP_MESSAGE_CHALLENGE;
    out[4..6].copy_from_slice(&t_id.to_le_bytes());
    out[6] = cval.len() as u8;
    out[8] = hashid;
    out[9] = dhgid;
    out[10..12].copy_from_slice(&(dhval.len() as u16).to_le_bytes());
    out[12..16].copy_from_slice(&seqnum.to_le_bytes());
    out[16..16 + cval.len()].copy_from_slice(cval);
    out[16 + cval.len()..].copy_from_slice(dhval);
    out
}

/// Parsed Challenge — used by the in-crate fake host. Production only
/// builds Challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeData {
    pub t_id: u16,
    pub hashid: u8,
    pub dhgid: u8,
    pub seqnum: u32,
    pub cval: Vec<u8>,
    pub dh_value: Vec<u8>,
}

/// Parse a Challenge message.
pub fn parse_challenge(buf: &[u8]) -> Result<ChallengeData, AuthWireError> {
    if buf.len() < DHCHAP_HEADER_LEN {
        return Err(AuthWireError::TooShort {
            got: buf.len(),
            need: DHCHAP_HEADER_LEN,
        });
    }
    expect_header(
        buf,
        NVME_AUTH_DHCHAP_MESSAGES,
        NVME_AUTH_DHCHAP_MESSAGE_CHALLENGE,
    )?;
    let t_id = u16::from_le_bytes([buf[4], buf[5]]);
    let hl = buf[6] as usize;
    let hashid = buf[8];
    let dhgid = buf[9];
    let dhvlen = u16::from_le_bytes([buf[10], buf[11]]) as usize;
    let seqnum = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
    let need = DHCHAP_HEADER_LEN + hl + dhvlen;
    if buf.len() < need {
        return Err(AuthWireError::TooShort {
            got: buf.len(),
            need,
        });
    }
    let cval = buf[16..16 + hl].to_vec();
    let dh_value = buf[16 + hl..16 + hl + dhvlen].to_vec();
    Ok(ChallengeData {
        t_id,
        hashid,
        dhgid,
        seqnum,
        cval,
        dh_value,
    })
}

// ===================== Reply (host -> controller) =====================

/// Parsed Reply message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyData {
    pub t_id: u16,
    /// Host response R1 (hash-len bytes).
    pub response: Vec<u8>,
    /// Host challenge C2 for mutual auth — present iff the host set
    /// `cvalid` (it supplied a `--dhchap-ctrl-secret`).
    pub host_challenge: Option<Vec<u8>>,
    /// Host DH public value g^y (`dhvlen` bytes; empty for NULL group).
    pub host_dh_value: Vec<u8>,
    /// Host sequence number S2 (used in the controller response HMAC).
    pub seqnum: u32,
}

/// Parse a Reply message (`auth_type = DHCHAP`, `auth_id = REPLY`).
/// `expected_hl` is the negotiated hash length; the message's own `hl`
/// field must match it. Layout after the 16-byte header:
/// `R1[hl]`, then `C2[hl]` iff `cvalid`, then `g^y[dhvlen]`.
pub fn parse_reply(buf: &[u8], expected_hl: usize) -> Result<ReplyData, AuthWireError> {
    if buf.len() < DHCHAP_HEADER_LEN {
        return Err(AuthWireError::TooShort {
            got: buf.len(),
            need: DHCHAP_HEADER_LEN,
        });
    }
    expect_header(
        buf,
        NVME_AUTH_DHCHAP_MESSAGES,
        NVME_AUTH_DHCHAP_MESSAGE_REPLY,
    )?;
    let t_id = u16::from_le_bytes([buf[4], buf[5]]);
    let hl = buf[6] as usize;
    if hl != expected_hl {
        return Err(AuthWireError::HashLenMismatch {
            got: hl,
            expected: expected_hl,
        });
    }
    let cvalid = (buf[8] & NVME_AUTH_DHCHAP_RESPONSE_VALID) != 0;
    let dhvlen = u16::from_le_bytes([buf[10], buf[11]]) as usize;
    let seqnum = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
    let c2_len = if cvalid { hl } else { 0 };
    let need = DHCHAP_HEADER_LEN + hl + c2_len + dhvlen;
    if buf.len() < need {
        return Err(AuthWireError::TooShort {
            got: buf.len(),
            need,
        });
    }
    let mut off = DHCHAP_HEADER_LEN;
    let response = buf[off..off + hl].to_vec();
    off += hl;
    let host_challenge = if cvalid {
        let c2 = buf[off..off + hl].to_vec();
        off += hl;
        Some(c2)
    } else {
        None
    };
    let host_dh_value = buf[off..off + dhvlen].to_vec();
    Ok(ReplyData {
        t_id,
        response,
        host_challenge,
        host_dh_value,
        seqnum,
    })
}

/// Encode a Reply — for the in-crate fake host / round-trip tests.
pub fn build_reply(
    t_id: u16,
    response: &[u8],
    host_challenge: Option<&[u8]>,
    seqnum: u32,
    host_dh_value: &[u8],
) -> Vec<u8> {
    let hl = response.len();
    let c2_len = host_challenge.map(|c| c.len()).unwrap_or(0);
    let mut out = vec![0u8; DHCHAP_HEADER_LEN + hl + c2_len + host_dh_value.len()];
    out[0] = NVME_AUTH_DHCHAP_MESSAGES;
    out[1] = NVME_AUTH_DHCHAP_MESSAGE_REPLY;
    out[4..6].copy_from_slice(&t_id.to_le_bytes());
    out[6] = hl as u8;
    if host_challenge.is_some() {
        out[8] = NVME_AUTH_DHCHAP_RESPONSE_VALID;
    }
    out[10..12].copy_from_slice(&(host_dh_value.len() as u16).to_le_bytes());
    out[12..16].copy_from_slice(&seqnum.to_le_bytes());
    let mut off = DHCHAP_HEADER_LEN;
    out[off..off + hl].copy_from_slice(response);
    off += hl;
    if let Some(c2) = host_challenge {
        out[off..off + c2.len()].copy_from_slice(c2);
        off += c2.len();
    }
    out[off..off + host_dh_value.len()].copy_from_slice(host_dh_value);
    out
}

// ===================== Success1 (controller -> host) =====================

/// Encode a Success1 message (`auth_type = DHCHAP`, `auth_id =
/// SUCCESS1`). `response` carries R2 for mutual auth (the host
/// requested it); `None` is unidirectional and sets `rvalid = 0`.
/// `hl` is the negotiated hash length, always set in the `hl` field.
pub fn build_success1(t_id: u16, hl: usize, response: Option<&[u8]>) -> Vec<u8> {
    let rval_len = response.map(|r| r.len()).unwrap_or(0);
    let mut out = vec![0u8; DHCHAP_HEADER_LEN + rval_len];
    out[0] = NVME_AUTH_DHCHAP_MESSAGES;
    out[1] = NVME_AUTH_DHCHAP_MESSAGE_SUCCESS1;
    out[4..6].copy_from_slice(&t_id.to_le_bytes());
    out[6] = hl as u8;
    if let Some(r) = response {
        out[8] = NVME_AUTH_DHCHAP_RESPONSE_VALID;
        out[DHCHAP_HEADER_LEN..DHCHAP_HEADER_LEN + r.len()].copy_from_slice(r);
    }
    out
}

/// Parsed Success1 — for the in-crate fake host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Success1Data {
    pub t_id: u16,
    /// Controller response R2 if the controller authenticated itself
    /// (mutual auth); `None` when `rvalid` was 0.
    pub response: Option<Vec<u8>>,
}

/// Parse a Success1 message.
pub fn parse_success1(buf: &[u8]) -> Result<Success1Data, AuthWireError> {
    if buf.len() < DHCHAP_HEADER_LEN {
        return Err(AuthWireError::TooShort {
            got: buf.len(),
            need: DHCHAP_HEADER_LEN,
        });
    }
    expect_header(
        buf,
        NVME_AUTH_DHCHAP_MESSAGES,
        NVME_AUTH_DHCHAP_MESSAGE_SUCCESS1,
    )?;
    let t_id = u16::from_le_bytes([buf[4], buf[5]]);
    let hl = buf[6] as usize;
    let rvalid = (buf[8] & NVME_AUTH_DHCHAP_RESPONSE_VALID) != 0;
    let response = if rvalid {
        if buf.len() < DHCHAP_HEADER_LEN + hl {
            return Err(AuthWireError::TooShort {
                got: buf.len(),
                need: DHCHAP_HEADER_LEN + hl,
            });
        }
        Some(buf[DHCHAP_HEADER_LEN..DHCHAP_HEADER_LEN + hl].to_vec())
    } else {
        None
    };
    Ok(Success1Data { t_id, response })
}

// ===================== Success2 (host -> controller) =====================

/// Encode a Success2 message — for the in-crate fake host.
pub fn build_success2(t_id: u16) -> Vec<u8> {
    let mut out = vec![0u8; DHCHAP_HEADER_LEN];
    out[0] = NVME_AUTH_DHCHAP_MESSAGES;
    out[1] = NVME_AUTH_DHCHAP_MESSAGE_SUCCESS2;
    out[4..6].copy_from_slice(&t_id.to_le_bytes());
    out
}

/// Parse a Success2 message; returns its transaction id.
pub fn parse_success2(buf: &[u8]) -> Result<u16, AuthWireError> {
    if buf.len() < DHCHAP_HEADER_LEN {
        return Err(AuthWireError::TooShort {
            got: buf.len(),
            need: DHCHAP_HEADER_LEN,
        });
    }
    expect_header(
        buf,
        NVME_AUTH_DHCHAP_MESSAGES,
        NVME_AUTH_DHCHAP_MESSAGE_SUCCESS2,
    )?;
    Ok(u16::from_le_bytes([buf[4], buf[5]]))
}

// ===================== Failure (either direction) =====================

/// Encode a Failure1 message (`auth_type = COMMON`, `auth_id =
/// FAILURE1`). `reason` is always [`NVME_AUTH_DHCHAP_FAILURE_REASON_FAILED`];
/// `reason_exp` is the explanation code.
pub fn build_failure1(t_id: u16, reason: u8, reason_exp: u8) -> Vec<u8> {
    let mut out = vec![0u8; COMMON_HEADER_LEN];
    out[0] = NVME_AUTH_COMMON_MESSAGES;
    out[1] = NVME_AUTH_DHCHAP_MESSAGE_FAILURE1;
    out[4..6].copy_from_slice(&t_id.to_le_bytes());
    out[6] = reason;
    out[7] = reason_exp;
    out
}

/// Parsed Failure message (host may send Failure2 to reject our R2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureData {
    pub t_id: u16,
    pub rescode: u8,
    pub rescode_exp: u8,
}

/// Parse a Failure message regardless of which side sent it
/// (`auth_type = COMMON`, `auth_id` = FAILURE1 or FAILURE2).
pub fn parse_failure(buf: &[u8]) -> Result<FailureData, AuthWireError> {
    if buf.len() < COMMON_HEADER_LEN {
        return Err(AuthWireError::TooShort {
            got: buf.len(),
            need: COMMON_HEADER_LEN,
        });
    }
    if buf[0] != NVME_AUTH_COMMON_MESSAGES {
        return Err(AuthWireError::BadAuthType {
            got: buf[0],
            expected: NVME_AUTH_COMMON_MESSAGES,
        });
    }
    if buf[1] != NVME_AUTH_DHCHAP_MESSAGE_FAILURE1 && buf[1] != NVME_AUTH_DHCHAP_MESSAGE_FAILURE2 {
        return Err(AuthWireError::BadMessageId {
            got: buf[1],
            expected: NVME_AUTH_DHCHAP_MESSAGE_FAILURE1,
        });
    }
    Ok(FailureData {
        t_id: u16::from_le_bytes([buf[4], buf[5]]),
        rescode: buf[6],
        rescode_exp: buf[7],
    })
}

/// Peek the `(auth_type, auth_id)` of a message without fully parsing
/// it — lets the state machine route an unexpected Failure mid-flow.
pub fn peek_message_type(buf: &[u8]) -> Option<(u8, u8)> {
    if buf.len() < 2 {
        return None;
    }
    Some((buf[0], buf[1]))
}

/// Read the transaction id (`t_id`) from any message header without a
/// full parse, returning 0 if the buffer is too short to contain it.
/// `t_id` sits at the same offset (4..6) in every message, so the
/// controller can echo the host's transaction id in an AUTH_Failure
/// even when the message body failed to parse.
pub fn peek_t_id(buf: &[u8]) -> u16 {
    if buf.len() >= 6 {
        u16::from_le_bytes([buf[4], buf[5]])
    } else {
        0
    }
}

fn expect_header(buf: &[u8], auth_type: u8, auth_id: u8) -> Result<(), AuthWireError> {
    if buf[0] != auth_type {
        return Err(AuthWireError::BadAuthType {
            got: buf[0],
            expected: auth_type,
        });
    }
    if buf[1] != auth_id {
        return Err(AuthWireError::BadMessageId {
            got: buf[1],
            expected: auth_id,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_len_table() {
        assert_eq!(hash_len(NVME_AUTH_HASH_SHA256), Some(32));
        assert_eq!(hash_len(NVME_AUTH_HASH_SHA384), Some(48));
        assert_eq!(hash_len(NVME_AUTH_HASH_SHA512), Some(64));
        assert_eq!(hash_len(0x00), None);
        assert_eq!(hash_len(NVME_AUTH_HASH_INVALID), None);
    }

    #[test]
    fn parse_auth_command_unpacks_cdw10_cdw11() {
        let mut bytes = vec![0u8; crate::SQE_SIZE];
        // CDW10 bytes (40..44) = [resv3, spsp0, spsp1, secp].
        bytes[40] = 0x00;
        bytes[41] = 0x01; // spsp0
        bytes[42] = 0x01; // spsp1
        bytes[43] = NVME_AUTH_DHCHAP_PROTOCOL_IDENTIFIER; // secp
        // CDW11 bytes (44..48) = TL/AL little-endian = 0x1234.
        bytes[44] = 0x34;
        bytes[45] = 0x12;
        let sqe = Sqe::parse(&bytes).unwrap();
        let f = parse_auth_command(&sqe);
        assert_eq!(f.spsp0, 0x01);
        assert_eq!(f.spsp1, 0x01);
        assert_eq!(f.secp, NVME_AUTH_DHCHAP_PROTOCOL_IDENTIFIER);
        assert_eq!(f.tl_al, 0x1234);
    }

    #[test]
    fn negotiate_round_trip() {
        let desc = ProtocolDescriptor {
            authid: NVME_AUTH_DHCHAP_AUTH_ID,
            hash_ids: vec![
                NVME_AUTH_HASH_SHA512,
                NVME_AUTH_HASH_SHA384,
                NVME_AUTH_HASH_SHA256,
            ],
            dhgroup_ids: vec![
                NVME_AUTH_DHGROUP_NULL,
                NVME_AUTH_DHGROUP_2048,
                NVME_AUTH_DHGROUP_8192,
            ],
        };
        let wire = build_negotiate(0xBEEF, 0, std::slice::from_ref(&desc));
        // Header 8 + one 64-byte descriptor.
        assert_eq!(wire.len(), 8 + 64);
        assert_eq!(wire[0], NVME_AUTH_COMMON_MESSAGES);
        assert_eq!(wire[1], NVME_AUTH_DHCHAP_MESSAGE_NEGOTIATE);
        assert_eq!(wire[7], 1); // napd
        let parsed = parse_negotiate(&wire).unwrap();
        assert_eq!(parsed.t_id, 0xBEEF);
        assert_eq!(parsed.sc_c, 0);
        assert_eq!(parsed.descriptors.len(), 1);
        assert_eq!(parsed.descriptors[0], desc);
        assert_eq!(parsed.dhchap_descriptor(), Some(&desc));
    }

    #[test]
    fn negotiate_rejects_wrong_auth_type() {
        let mut wire = build_negotiate(1, 0, &[]);
        wire[0] = NVME_AUTH_DHCHAP_MESSAGES; // wrong namespace
        assert!(matches!(
            parse_negotiate(&wire),
            Err(AuthWireError::BadAuthType { .. })
        ));
    }

    #[test]
    fn negotiate_rejects_truncated_descriptor() {
        let mut wire = build_negotiate(1, 0, &[]);
        wire[7] = 1; // claims 1 descriptor but none follows
        assert!(matches!(
            parse_negotiate(&wire),
            Err(AuthWireError::TooShort { .. })
        ));
    }

    #[test]
    fn challenge_round_trip_with_dh() {
        let cval = vec![0xAB; 48]; // SHA-384 challenge
        let dhval = vec![0xCD; 256]; // ffdhe2048 public value
        let wire = build_challenge(
            0x1357,
            NVME_AUTH_HASH_SHA384,
            NVME_AUTH_DHGROUP_2048,
            0xDEADBEEF,
            &cval,
            &dhval,
        );
        assert_eq!(wire.len(), 16 + 48 + 256);
        let p = parse_challenge(&wire).unwrap();
        assert_eq!(p.t_id, 0x1357);
        assert_eq!(p.hashid, NVME_AUTH_HASH_SHA384);
        assert_eq!(p.dhgid, NVME_AUTH_DHGROUP_2048);
        assert_eq!(p.seqnum, 0xDEADBEEF);
        assert_eq!(p.cval, cval);
        assert_eq!(p.dh_value, dhval);
    }

    #[test]
    fn challenge_round_trip_null_group() {
        let cval = vec![0x11; 32];
        let wire = build_challenge(
            7,
            NVME_AUTH_HASH_SHA256,
            NVME_AUTH_DHGROUP_NULL,
            1,
            &cval,
            &[],
        );
        assert_eq!(wire.len(), 16 + 32);
        let p = parse_challenge(&wire).unwrap();
        assert!(p.dh_value.is_empty());
        assert_eq!(p.cval, cval);
    }

    #[test]
    fn reply_round_trip_unidirectional() {
        let r1 = vec![0x22; 32];
        let wire = build_reply(9, &r1, None, 0x44, &[]);
        assert_eq!(wire.len(), 16 + 32);
        let p = parse_reply(&wire, 32).unwrap();
        assert_eq!(p.t_id, 9);
        assert_eq!(p.response, r1);
        assert_eq!(p.host_challenge, None);
        assert!(p.host_dh_value.is_empty());
        assert_eq!(p.seqnum, 0x44);
    }

    #[test]
    fn reply_round_trip_bidirectional_with_dh() {
        let r1 = vec![0x22; 64]; // SHA-512
        let c2 = vec![0x33; 64];
        let dhy = vec![0x55; 384]; // ffdhe3072
        let wire = build_reply(9, &r1, Some(&c2), 0x66, &dhy);
        assert_eq!(wire.len(), 16 + 64 + 64 + 384);
        let p = parse_reply(&wire, 64).unwrap();
        assert_eq!(p.response, r1);
        assert_eq!(p.host_challenge, Some(c2));
        assert_eq!(p.host_dh_value, dhy);
    }

    #[test]
    fn reply_rejects_hash_len_mismatch() {
        let wire = build_reply(1, &[0u8; 32], None, 0, &[]);
        assert!(matches!(
            parse_reply(&wire, 48),
            Err(AuthWireError::HashLenMismatch {
                got: 32,
                expected: 48
            })
        ));
    }

    #[test]
    fn success1_round_trip_mutual_and_unidirectional() {
        // Mutual: carries R2.
        let r2 = vec![0x77; 48];
        let wire = build_success1(3, 48, Some(&r2));
        assert_eq!(wire.len(), 16 + 48);
        assert_eq!(
            wire[8] & NVME_AUTH_DHCHAP_RESPONSE_VALID,
            NVME_AUTH_DHCHAP_RESPONSE_VALID
        );
        let p = parse_success1(&wire).unwrap();
        assert_eq!(p.t_id, 3);
        assert_eq!(p.response, Some(r2));

        // Unidirectional: no R2, rvalid clear.
        let wire2 = build_success1(3, 48, None);
        assert_eq!(wire2.len(), 16);
        assert_eq!(wire2[8] & NVME_AUTH_DHCHAP_RESPONSE_VALID, 0);
        assert_eq!(parse_success1(&wire2).unwrap().response, None);
    }

    #[test]
    fn success2_round_trip() {
        let wire = build_success2(0x9999);
        assert_eq!(wire.len(), 16);
        assert_eq!(parse_success2(&wire).unwrap(), 0x9999);
    }

    #[test]
    fn failure_round_trip() {
        let wire = build_failure1(
            0x1234,
            NVME_AUTH_DHCHAP_FAILURE_REASON_FAILED,
            NVME_AUTH_DHCHAP_FAILURE_FAILED,
        );
        assert_eq!(wire.len(), 8);
        let p = parse_failure(&wire).unwrap();
        assert_eq!(p.t_id, 0x1234);
        assert_eq!(p.rescode, NVME_AUTH_DHCHAP_FAILURE_REASON_FAILED);
        assert_eq!(p.rescode_exp, NVME_AUTH_DHCHAP_FAILURE_FAILED);
    }

    #[test]
    fn peek_message_type_reads_header() {
        let wire = build_success2(1);
        assert_eq!(
            peek_message_type(&wire),
            Some((NVME_AUTH_DHCHAP_MESSAGES, NVME_AUTH_DHCHAP_MESSAGE_SUCCESS2))
        );
        assert_eq!(peek_message_type(&[0u8]), None);
    }

    #[test]
    fn peek_t_id_recovers_offset_or_zero() {
        // A Negotiate header carries t_id at bytes 4..6.
        let wire = build_negotiate(0xABCD, 0, &[]);
        assert_eq!(peek_t_id(&wire), 0xABCD);
        // Too short to reach the t_id field -> 0.
        assert_eq!(peek_t_id(&[0u8; 4]), 0);
        assert_eq!(peek_t_id(&[]), 0);
    }
}
