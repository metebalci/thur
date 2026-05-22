// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVMe/TCP PDU types (NVMe-oF TCP Transport Spec §3).
//!
//! Wire layout summary:
//!
//! ```text
//! +----------+----+----+----+--------+
//! | PDU type | FL | HL | PDA|  PLEN  |
//! +----------+----+----+----+--------+
//! |          PDU-specific header     |   <- bytes 8..HL
//! +----------------------------------+
//! |   [opt] header digest (CRC32C)   |
//! +----------------------------------+
//! |   [opt] PDO padding              |
//! +----------------------------------+
//! |               data               |
//! +----------------------------------+
//! |   [opt] data digest (CRC32C)     |
//! +----------------------------------+
//! ```
//!
//! - **PDU type** (1 byte) — selects the PDU variant ([`PduType`]).
//! - **FL** (1 byte) — flags. Bit 0 = header digest present
//!   ([`FLAGS_HDGSTF`]); bit 1 = data digest present ([`FLAGS_DDGSTF`]).
//!   Other bits PDU-type-specific (e.g. C2HData bits 2 / 3 are
//!   LAST_PDU and SUCCESS).
//! - **HL** (1 byte) — header length in **bytes**, INCLUDING the
//!   common 8-byte header but EXCLUDING the optional digest fields.
//!   For ICReq: 8 + 120 = 128; for CapsuleCmd: 8 + 64 = 72; for
//!   CapsuleResp: 8 + 16 = 24; for C2HData: 8 + 16 = 24.
//! - **PDA / PDO** (1 byte) — Pad Data Offset. Byte offset from the
//!   *start of the PDU* at which the data payload begins. 0 means
//!   "no data" (or "right after the header digest"); otherwise the
//!   gap between HL (+digest if present) and PDO is implementation
//!   padding the host may inspect or ignore.
//! - **PLEN** (4 bytes, little-endian) — total PDU length in bytes,
//!   including header + digests + payload.
//!
//! # Digest negotiation
//!
//! Digests are negotiated in ICReq / ICResp; until both sides agree
//! to enable them (DGST bits 0 / 1), the flags bits remain 0 on every
//! PDU. The MVP server in this crate always negotiates 0 (no
//! digests), so the codec keeps the parse paths simple — digest
//! bytes are not allocated and not verified.

use bytes::{Buf, BufMut};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::pdu::error::PduError;
use nvme_base::{Cqe, Sqe};

pub mod error {
    use thiserror::Error;

    #[derive(Debug, Error)]
    pub enum PduError {
        #[error("PDU header too short: need 8 bytes, got {0}")]
        HeaderShort(usize),
        #[error("unknown PDU type 0x{0:02X}")]
        UnknownPdu(u8),
        #[error("PDU HLEN {hlen} must be >= 8 (common header)")]
        HlenTooSmall { hlen: u8 },
        #[error("PDU PLEN {plen} smaller than HLEN {hlen}")]
        PduLengthInvalid { plen: u32, hlen: u8 },
        #[error("PDU PLEN {0} exceeds receive cap {1}")]
        PduTooLarge(u32, u32),
        #[error("ICReq payload length must be 120 bytes, got {0}")]
        IcReqLength(usize),
        #[error("ICResp payload length must be 120 bytes, got {0}")]
        IcRespLength(usize),
        #[error("CapsuleCmd HLEN must be at least 72 (8 + SQE), got {0}")]
        CapsuleCmdShort(u8),
        #[error("H2CData HLEN must be at least 24 (8 + h2cdata header), got {0}")]
        H2CDataShort(u8),
        #[error("H2CData DATAL field {datal} disagrees with actual data slice length {actual}")]
        H2CDataLengthMismatch { datal: u32, actual: usize },
        #[error("PDO {pdo} outside PDU bounds (PLEN {plen})")]
        PdoOutOfBounds { pdo: u8, plen: u32 },
        #[error("I/O error reading PDU: {0}")]
        Io(#[from] std::io::Error),
        #[error("base SQE parse: {0}")]
        Sqe(#[from] nvme_base::NvmeError),
    }
}

/// Receive-side hard cap on PDU size. Hosts that send anything
/// larger get a protocol error instead of letting us allocate
/// arbitrary memory. 256 KiB comfortably covers ICReq (128 B),
/// CapsuleResp (24 B), and the largest in-capsule write the
/// Identify Controller's IOCCSZ advertises (~16 KiB).
pub const MAX_PDU_BYTES: u32 = 256 * 1024;

/// Header-digest flag bit (FL[0]).
pub const FLAGS_HDGSTF: u8 = 1 << 0;
/// Data-digest flag bit (FL[1]).
pub const FLAGS_DDGSTF: u8 = 1 << 1;
/// C2HData: this is the final C2HData PDU of a multi-PDU response.
pub const C2H_FLAGS_LAST_PDU: u8 = 1 << 2;
/// C2HData: treat as implicit success completion (no CapsuleResp
/// follows). The MVP server does not set this — it always emits a
/// separate CapsuleResp after data.
pub const C2H_FLAGS_SUCCESS: u8 = 1 << 3;
/// H2CData: this is the final H2CData PDU servicing the R2T.
/// Same bit position as `C2H_FLAGS_LAST_PDU`; aliased for symmetry
/// of intent at the call site.
pub const H2C_FLAGS_LAST_PDU: u8 = 1 << 2;

/// PDU type field (NVMe/TCP §3.4 Figure 13).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PduType {
    ICReq = 0x00,
    ICResp = 0x01,
    H2CTermReq = 0x02,
    C2HTermReq = 0x03,
    CapsuleCmd = 0x04,
    CapsuleResp = 0x05,
    H2CData = 0x06,
    C2HData = 0x07,
    R2T = 0x09,
}

impl PduType {
    pub fn from_u8(b: u8) -> Result<Self, PduError> {
        Ok(match b {
            0x00 => Self::ICReq,
            0x01 => Self::ICResp,
            0x02 => Self::H2CTermReq,
            0x03 => Self::C2HTermReq,
            0x04 => Self::CapsuleCmd,
            0x05 => Self::CapsuleResp,
            0x06 => Self::H2CData,
            0x07 => Self::C2HData,
            0x09 => Self::R2T,
            other => return Err(PduError::UnknownPdu(other)),
        })
    }
}

/// Common 8-byte PDU header.
#[derive(Debug, Clone, Copy)]
pub struct CommonHeader {
    pub pdu_type: PduType,
    /// FL — flag byte. See [`FLAGS_HDGSTF`] / [`FLAGS_DDGSTF`] /
    /// [`C2H_FLAGS_LAST_PDU`] / [`C2H_FLAGS_SUCCESS`].
    pub flags: u8,
    /// HL — header length in bytes, INCLUDING the 8-byte common
    /// header. Excludes header digest.
    pub hlen: u8,
    /// PDO — Pad Data Offset. Byte offset from the start of the
    /// PDU at which the data payload begins.
    pub pdo: u8,
    /// PLEN — total PDU length in bytes (header + digests + data).
    pub plen: u32,
}

impl CommonHeader {
    pub const WIRE_LEN: usize = 8;

    pub fn write_to(&self, buf: &mut impl BufMut) {
        buf.put_u8(self.pdu_type as u8);
        buf.put_u8(self.flags);
        buf.put_u8(self.hlen);
        buf.put_u8(self.pdo);
        buf.put_u32_le(self.plen);
    }

    pub fn read_from(buf: &[u8]) -> Result<Self, PduError> {
        if buf.len() < Self::WIRE_LEN {
            return Err(PduError::HeaderShort(buf.len()));
        }
        let pdu_type = PduType::from_u8(buf[0])?;
        let flags = buf[1];
        let hlen = buf[2];
        let pdo = buf[3];
        let plen = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        if (hlen as usize) < Self::WIRE_LEN {
            return Err(PduError::HlenTooSmall { hlen });
        }
        if plen < u32::from(hlen) {
            return Err(PduError::PduLengthInvalid { plen, hlen });
        }
        Ok(Self {
            pdu_type,
            flags,
            hlen,
            pdo,
            plen,
        })
    }
}

/// Initialize Connection Request (NVMe/TCP §3.6.1). First PDU on
/// every freshly-accepted TCP connection. Total PDU = 128 bytes
/// (8-byte common header + 120-byte payload).
#[derive(Debug, Clone, Copy)]
pub struct ICReq {
    pub pfv: u16,
    pub hpda: u8,
    pub dgst: u8,
    pub maxr2t: u32,
}

impl ICReq {
    pub const PAYLOAD_LEN: usize = 120;
    /// Total ICReq PDU wire size: 8-byte common header + 120-byte
    /// payload = 128 bytes (per NVMe-TCP §3.6.2). No digests
    /// (negotiation hasn't happened yet).
    pub const PDU_LEN: u32 = (CommonHeader::WIRE_LEN + Self::PAYLOAD_LEN) as u32;

    pub fn write_to(&self, buf: &mut impl BufMut) {
        buf.put_u16_le(self.pfv);
        buf.put_u8(self.hpda);
        buf.put_u8(self.dgst);
        buf.put_u32_le(self.maxr2t);
        for _ in 8..Self::PAYLOAD_LEN {
            buf.put_u8(0);
        }
    }

    pub fn read_from(buf: &[u8]) -> Result<Self, PduError> {
        if buf.len() != Self::PAYLOAD_LEN {
            return Err(PduError::IcReqLength(buf.len()));
        }
        let mut b = buf;
        let pfv = b.get_u16_le();
        let hpda = b.get_u8();
        let dgst = b.get_u8();
        let maxr2t = b.get_u32_le();
        Ok(Self {
            pfv,
            hpda,
            dgst,
            maxr2t,
        })
    }
}

/// Initialize Connection Response (NVMe/TCP §3.6.2).
#[derive(Debug, Clone, Copy)]
pub struct ICResp {
    pub pfv: u16,
    pub cpda: u8,
    pub dgst: u8,
    pub maxh2cdata: u32,
}

impl ICResp {
    pub const PAYLOAD_LEN: usize = 120;
    pub const PDU_LEN: u32 = (CommonHeader::WIRE_LEN + Self::PAYLOAD_LEN) as u32;

    pub fn write_to(&self, buf: &mut impl BufMut) {
        buf.put_u16_le(self.pfv);
        buf.put_u8(self.cpda);
        buf.put_u8(self.dgst);
        buf.put_u32_le(self.maxh2cdata);
        for _ in 8..Self::PAYLOAD_LEN {
            buf.put_u8(0);
        }
    }

    pub fn read_from(buf: &[u8]) -> Result<Self, PduError> {
        if buf.len() != Self::PAYLOAD_LEN {
            return Err(PduError::IcRespLength(buf.len()));
        }
        let mut b = buf;
        let pfv = b.get_u16_le();
        let cpda = b.get_u8();
        let dgst = b.get_u8();
        let maxh2cdata = b.get_u32_le();
        Ok(Self {
            pfv,
            cpda,
            dgst,
            maxh2cdata,
        })
    }

    /// Encode as a full PDU (common header + payload, no digests).
    pub fn to_pdu(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(Self::PDU_LEN as usize);
        let header = CommonHeader {
            pdu_type: PduType::ICResp,
            flags: 0,
            hlen: (CommonHeader::WIRE_LEN + Self::PAYLOAD_LEN) as u8,
            pdo: 0,
            plen: Self::PDU_LEN,
        };
        header.write_to(&mut buf);
        self.write_to(&mut buf);
        buf
    }
}

/// Encode a CapsuleResp PDU (NVMe/TCP §3.6.6): common header + 16-byte
/// CQE. No data, no digests.
pub fn build_capsule_resp_pdu(cqe: &Cqe) -> Vec<u8> {
    const HLEN: u8 = (CommonHeader::WIRE_LEN + nvme_base::CQE_SIZE) as u8;
    let plen = HLEN as u32;
    let mut buf = Vec::with_capacity(plen as usize);
    let header = CommonHeader {
        pdu_type: PduType::CapsuleResp,
        flags: 0,
        hlen: HLEN,
        pdo: 0,
        plen,
    };
    header.write_to(&mut buf);
    buf.extend_from_slice(&cqe.to_bytes());
    buf
}

/// Encode a C2HData PDU (NVMe/TCP §3.6.7) for a single-PDU read
/// response. Flags = LAST_PDU only — the receive side is expected
/// to wait for a separate CapsuleResp carrying the CQE. See
/// [`build_c2hdata_pdu_with_flags`] for the SUCCESS-bit
/// optimization.
///
/// Layout: common header (8) + C2HData-specific header (16) + data
/// (no digests). Pdo = HLEN since there's no header digest gap.
pub fn build_c2hdata_pdu(cccid: u16, data: &[u8]) -> Vec<u8> {
    build_c2hdata_pdu_with_flags(cccid, data, C2H_FLAGS_LAST_PDU)
}

/// Encode a C2HData PDU with caller-chosen extra flags. The codec
/// always sets `LAST_PDU` because every C2HData this server emits
/// is the final (and only) data PDU for its command; callers can
/// additionally set [`C2H_FLAGS_SUCCESS`] to fold the CQE into the
/// C2HData and skip the trailing CapsuleResp (NVMe/TCP §3.6.7).
pub fn build_c2hdata_pdu_with_flags(cccid: u16, data: &[u8], extra_flags: u8) -> Vec<u8> {
    const HLEN: u8 = (CommonHeader::WIRE_LEN + 16) as u8; // 24
    let plen = u32::from(HLEN) + data.len() as u32;
    let mut buf = Vec::with_capacity(plen as usize);
    let header = CommonHeader {
        pdu_type: PduType::C2HData,
        flags: C2H_FLAGS_LAST_PDU | extra_flags,
        hlen: HLEN,
        pdo: HLEN,
        plen,
    };
    header.write_to(&mut buf);
    buf.extend_from_slice(&cccid.to_le_bytes());
    buf.extend_from_slice(&[0u8; 2]); // reserved
    buf.extend_from_slice(&0u32.to_le_bytes()); // DATAO = 0
    buf.extend_from_slice(&(data.len() as u32).to_le_bytes()); // DATAL
    buf.extend_from_slice(&[0u8; 4]); // reserved
    buf.extend_from_slice(data);
    buf
}

/// Encode an R2T PDU (NVMe/TCP §3.6.8). Used by the controller to
/// solicit host-to-controller data when a write command's data
/// payload was not (fully) carried in the CapsuleCmd's in-capsule
/// data area.
///
/// PDU layout: common header (8) + R2T-specific header (16):
///   bytes 0..2   CCCID  (the originating command's CID)
///   bytes 2..4   TTAG   (Transfer Tag, unique per outstanding R2T
///                        for this command — host echoes in its
///                        H2CData PDUs)
///   bytes 4..8   R2TO   (Requested Offset, into the command's
///                        total transfer)
///   bytes 8..12  R2TL   (Requested Length, in bytes)
///   bytes 12..16 reserved
pub fn build_r2t_pdu(cccid: u16, ttag: u16, r2to: u32, r2tl: u32) -> Vec<u8> {
    const HLEN: u8 = (CommonHeader::WIRE_LEN + 16) as u8; // 24
    let plen = u32::from(HLEN);
    let mut buf = Vec::with_capacity(plen as usize);
    let header = CommonHeader {
        pdu_type: PduType::R2T,
        flags: 0,
        hlen: HLEN,
        pdo: 0,
        plen,
    };
    header.write_to(&mut buf);
    buf.extend_from_slice(&cccid.to_le_bytes());
    buf.extend_from_slice(&ttag.to_le_bytes());
    buf.extend_from_slice(&r2to.to_le_bytes());
    buf.extend_from_slice(&r2tl.to_le_bytes());
    buf.extend_from_slice(&[0u8; 4]); // reserved
    buf
}

/// Encode a C2HTermReq PDU (NVMe/TCP §3.6.4). Used to tell the host
/// the controller is closing the connection because of a fatal
/// protocol violation. `fes` is the Fatal Error Status; common values:
/// 0x01 Invalid PDU Header Field, 0x02 PDU Sequence Error,
/// 0x07 Invalid PDU Header Type.
///
/// PDU layout: common header (8) + 24 byte type-specific header. The
/// type-specific header is FES (2) + FEI (4) + reserved (10) + the
/// rejected header (8). We zero the rejected-header bytes; the host
/// still gets enough information from FES to log the violation.
pub fn build_c2h_term_req_pdu(fes: u16) -> Vec<u8> {
    const HLEN: u8 = (CommonHeader::WIRE_LEN + 24) as u8; // 32
    let plen = u32::from(HLEN);
    let mut buf = Vec::with_capacity(plen as usize);
    let header = CommonHeader {
        pdu_type: PduType::C2HTermReq,
        flags: 0,
        hlen: HLEN,
        pdo: 0,
        plen,
    };
    header.write_to(&mut buf);
    buf.extend_from_slice(&fes.to_le_bytes());
    buf.extend_from_slice(&[0u8; 4]); // FEI
    buf.extend_from_slice(&[0u8; 10]); // reserved
    buf.extend_from_slice(&[0u8; 8]); // rejected header bytes (zeroed)
    buf
}

/// Raw PDU after read: parsed common header + everything that
/// followed it (HL-8 type-specific bytes + optional header digest +
/// PDO pad + data + optional data digest).
pub struct RawPdu {
    pub header: CommonHeader,
    /// Bytes after the common header. Length = PLEN - 8. Parsers
    /// reach into this via offsets derived from HL + flags + PDO.
    pub body: Vec<u8>,
}

impl RawPdu {
    /// Read one full PDU from an async stream. Returns `Io` on socket
    /// error (including clean EOF on the very first read of the
    /// header — the caller distinguishes "host hung up" from real
    /// errors).
    pub async fn read_async<R: AsyncRead + Unpin>(stream: &mut R) -> Result<Self, PduError> {
        let mut header_buf = [0u8; CommonHeader::WIRE_LEN];
        stream.read_exact(&mut header_buf).await?;
        let header = CommonHeader::read_from(&header_buf)?;
        if header.plen > MAX_PDU_BYTES {
            return Err(PduError::PduTooLarge(header.plen, MAX_PDU_BYTES));
        }
        let body_len = header.plen as usize - CommonHeader::WIRE_LEN;
        let mut body = vec![0u8; body_len];
        if body_len > 0 {
            stream.read_exact(&mut body).await?;
        }
        Ok(Self { header, body })
    }

    /// Pull the in-capsule data slice (if any). Data starts at PDO
    /// (offset from PDU start) and ends at PLEN minus the data digest
    /// (4 bytes if DDGSTF set, else 0). PDO=0 means "no data". Returns
    /// `None` for no data, `Some(&[])` for an empty-data PDU.
    pub fn in_capsule_data(&self) -> Result<Option<&[u8]>, PduError> {
        if self.header.pdo == 0 {
            return Ok(None);
        }
        if u32::from(self.header.pdo) > self.header.plen {
            return Err(PduError::PdoOutOfBounds {
                pdo: self.header.pdo,
                plen: self.header.plen,
            });
        }
        let data_digest_len = if self.header.flags & FLAGS_DDGSTF != 0 {
            4
        } else {
            0
        };
        let data_start_in_pdu = self.header.pdo as usize;
        let data_end_in_pdu = self.header.plen as usize - data_digest_len;
        if data_end_in_pdu < data_start_in_pdu {
            return Ok(Some(&[]));
        }
        let data_start_in_body = data_start_in_pdu - CommonHeader::WIRE_LEN;
        let data_end_in_body = data_end_in_pdu - CommonHeader::WIRE_LEN;
        Ok(Some(&self.body[data_start_in_body..data_end_in_body]))
    }
}

/// Decode a CapsuleCmd PDU into its SQE plus the in-capsule data
/// slice (if any). Caller owns the [`RawPdu`]; the SQE is decoded
/// out of `body[0..64]` and the data slice (when present) is
/// borrowed from `body`.
pub fn parse_capsule_cmd(pdu: &RawPdu) -> Result<(Sqe, Option<&[u8]>), PduError> {
    if pdu.header.pdu_type != PduType::CapsuleCmd {
        return Err(PduError::UnknownPdu(pdu.header.pdu_type as u8));
    }
    if pdu.header.hlen < (CommonHeader::WIRE_LEN + nvme_base::SQE_SIZE) as u8 {
        return Err(PduError::CapsuleCmdShort(pdu.header.hlen));
    }
    let sqe = Sqe::parse(&pdu.body[0..nvme_base::SQE_SIZE])?;
    let data = pdu.in_capsule_data()?;
    Ok((sqe, data))
}

/// Decoded H2CData (host-to-controller data PDU, NVMe/TCP §3.6.5).
/// Borrows the data slice from the [`RawPdu`] body.
#[derive(Debug)]
pub struct H2CData<'a> {
    pub cccid: u16,
    pub ttag: u16,
    /// DATAO — byte offset within the command's total transfer.
    pub datao: u32,
    /// DATAL — payload length in this PDU (= `data.len()`).
    pub datal: u32,
    /// LAST_PDU flag set on the final H2CData of an R2T fulfillment.
    pub last_pdu: bool,
    pub data: &'a [u8],
}

/// Decode an H2CData PDU. Validates the type-specific header is
/// well-formed and that DATAL matches the actual data slice length
/// (host bug if it doesn't).
pub fn parse_h2cdata(pdu: &RawPdu) -> Result<H2CData<'_>, PduError> {
    if pdu.header.pdu_type != PduType::H2CData {
        return Err(PduError::UnknownPdu(pdu.header.pdu_type as u8));
    }
    if pdu.header.hlen < (CommonHeader::WIRE_LEN + 16) as u8 {
        return Err(PduError::H2CDataShort(pdu.header.hlen));
    }
    let cccid = u16::from_le_bytes([pdu.body[0], pdu.body[1]]);
    let ttag = u16::from_le_bytes([pdu.body[2], pdu.body[3]]);
    let datao = u32::from_le_bytes([pdu.body[4], pdu.body[5], pdu.body[6], pdu.body[7]]);
    let datal = u32::from_le_bytes([pdu.body[8], pdu.body[9], pdu.body[10], pdu.body[11]]);
    let data = pdu.in_capsule_data()?.unwrap_or(&[]);
    if data.len() as u32 != datal {
        return Err(PduError::H2CDataLengthMismatch {
            datal,
            actual: data.len(),
        });
    }
    Ok(H2CData {
        cccid,
        ttag,
        datao,
        datal,
        last_pdu: (pdu.header.flags & H2C_FLAGS_LAST_PDU) != 0,
        data,
    })
}

/// Total transfer length declared in the SQE's Data Pointer SGL
/// descriptor (NVMe Base §4.4 SGL Data Block).
///
/// NVMe/TCP always uses SGLs (`Psdt::SglInline`); the length field is
/// at bytes 8..12 of the 16-byte SGL descriptor regardless of the
/// descriptor type / subtype (Data Block, Transport SGL Data Block
/// for in-capsule, ...). Returning u32 matches the on-wire width.
pub fn sgl_data_length(sqe: &Sqe) -> u32 {
    u32::from_le_bytes([sqe.dptr[8], sqe.dptr[9], sqe.dptr[10], sqe.dptr[11]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_header_round_trip() {
        let h = CommonHeader {
            pdu_type: PduType::ICReq,
            flags: 0,
            hlen: 128, // 8 common + 120 ICReq payload
            pdo: 0,
            plen: 128,
        };
        let mut buf = Vec::with_capacity(8);
        h.write_to(&mut buf);
        assert_eq!(buf.len(), 8);
        let parsed = CommonHeader::read_from(&buf).expect("parse");
        assert_eq!(parsed.pdu_type, PduType::ICReq);
        assert_eq!(parsed.hlen, 128);
        assert_eq!(parsed.plen, 128);
    }

    #[test]
    fn icreq_round_trip() {
        let r = ICReq {
            pfv: 0,
            hpda: 0,
            dgst: 0b11,
            maxr2t: 16,
        };
        let mut buf = Vec::with_capacity(ICReq::PAYLOAD_LEN);
        r.write_to(&mut buf);
        let parsed = ICReq::read_from(&buf).expect("parse");
        assert_eq!(parsed.maxr2t, 16);
        assert_eq!(parsed.dgst, 0b11);
    }

    #[test]
    fn unknown_pdu_type_errors() {
        // Use HLEN ≥ 8 so the type check fires before length check.
        let mut buf = vec![0u8; 8];
        buf[0] = 0xFF;
        buf[2] = 8;
        buf[4..8].copy_from_slice(&8u32.to_le_bytes());
        let err = CommonHeader::read_from(&buf).unwrap_err();
        assert!(matches!(err, PduError::UnknownPdu(0xFF)));
    }

    #[test]
    fn hlen_below_common_header_errors() {
        let mut buf = vec![0u8; 8];
        buf[0] = PduType::CapsuleResp as u8;
        buf[2] = 4; // HLEN < 8 — invalid
        buf[4..8].copy_from_slice(&8u32.to_le_bytes());
        let err = CommonHeader::read_from(&buf).unwrap_err();
        assert!(matches!(err, PduError::HlenTooSmall { hlen: 4 }));
    }

    #[test]
    fn plen_below_hlen_errors() {
        let mut buf = vec![0u8; 8];
        buf[0] = PduType::CapsuleResp as u8;
        buf[2] = 24;
        buf[4..8].copy_from_slice(&20u32.to_le_bytes()); // PLEN < HLEN
        let err = CommonHeader::read_from(&buf).unwrap_err();
        assert!(matches!(err, PduError::PduLengthInvalid { .. }));
    }

    #[test]
    fn icresp_to_pdu_has_correct_shape() {
        let r = ICResp {
            pfv: 0,
            cpda: 0,
            dgst: 0,
            maxh2cdata: 128 * 1024,
        };
        let pdu = r.to_pdu();
        // NVMe-TCP §3.6.2: ICResp PDU is 128 bytes total
        // (8-byte common header + 120-byte payload).
        assert_eq!(pdu.len(), 128);
        let header = CommonHeader::read_from(&pdu[..8]).unwrap();
        assert_eq!(header.pdu_type, PduType::ICResp);
        assert_eq!(header.hlen, 128);
        assert_eq!(header.plen, 128);
        let payload = ICResp::read_from(&pdu[8..128]).unwrap();
        assert_eq!(payload.maxh2cdata, 128 * 1024);
    }

    #[test]
    fn capsule_resp_pdu_carries_cqe() {
        let cqe = Cqe::success(0x1234, 0, 7, 0xDEAD_BEEF);
        let pdu = build_capsule_resp_pdu(&cqe);
        assert_eq!(pdu.len(), 24);
        let header = CommonHeader::read_from(&pdu[..8]).unwrap();
        assert_eq!(header.pdu_type, PduType::CapsuleResp);
        assert_eq!(header.hlen, 24);
        assert_eq!(header.plen, 24);
        // CID echoed at bytes 8+12..8+14
        assert_eq!(&pdu[20..22], &0x1234u16.to_le_bytes());
        // DW0 at bytes 8+0..8+4
        assert_eq!(&pdu[8..12], &0xDEAD_BEEFu32.to_le_bytes());
    }

    #[test]
    fn c2hdata_pdu_layout() {
        let data = [0x11u8, 0x22, 0x33, 0x44];
        let pdu = build_c2hdata_pdu(0x55AA, &data);
        // 8 common + 16 c2hdata-specific + 4 data = 28
        assert_eq!(pdu.len(), 28);
        let header = CommonHeader::read_from(&pdu[..8]).unwrap();
        assert_eq!(header.pdu_type, PduType::C2HData);
        assert_eq!(header.hlen, 24);
        assert_eq!(header.pdo, 24);
        assert_eq!(header.plen, 28);
        assert_eq!(header.flags & C2H_FLAGS_LAST_PDU, C2H_FLAGS_LAST_PDU);
        assert_eq!(header.flags & C2H_FLAGS_SUCCESS, 0);
        // CCCID at body[0..2]
        assert_eq!(&pdu[8..10], &0x55AAu16.to_le_bytes());
        // DATAL at body[8..12]
        assert_eq!(&pdu[16..20], &4u32.to_le_bytes());
        // data after PDO
        assert_eq!(&pdu[24..28], &data);
    }

    #[test]
    fn parse_capsule_cmd_extracts_sqe_and_data() {
        // Build a CapsuleCmd carrying a 64 byte SQE + 16 bytes ICD.
        const HLEN: u8 = 72;
        const PDO: u8 = 72; // data immediately after SQE
        let mut body = vec![0u8; 64];
        body[0] = 0x06; // OPC = Identify
        body[2] = 0x03; // CID = 3
        body[4] = 0x01; // NSID = 1
        body.extend_from_slice(&[0xAAu8; 16]);
        let mut pdu_buf = Vec::with_capacity(8 + body.len());
        let header = CommonHeader {
            pdu_type: PduType::CapsuleCmd,
            flags: 0,
            hlen: HLEN,
            pdo: PDO,
            plen: 8 + body.len() as u32,
        };
        header.write_to(&mut pdu_buf);
        pdu_buf.extend_from_slice(&body);
        let raw = RawPdu {
            header,
            body: pdu_buf[8..].to_vec(),
        };
        let (sqe, data) = parse_capsule_cmd(&raw).expect("parse");
        assert_eq!(sqe.opcode, 0x06);
        assert_eq!(sqe.cid, 3);
        assert_eq!(sqe.nsid, 1);
        assert_eq!(data.unwrap(), &[0xAAu8; 16]);
    }

    #[test]
    fn r2t_pdu_layout() {
        let pdu = build_r2t_pdu(0x1234, 0xABCD, 0, 65536);
        assert_eq!(pdu.len(), 24);
        let header = CommonHeader::read_from(&pdu[..8]).unwrap();
        assert_eq!(header.pdu_type, PduType::R2T);
        assert_eq!(header.hlen, 24);
        assert_eq!(header.pdo, 0);
        assert_eq!(header.plen, 24);
        // CCCID
        assert_eq!(&pdu[8..10], &0x1234u16.to_le_bytes());
        // TTAG
        assert_eq!(&pdu[10..12], &0xABCDu16.to_le_bytes());
        // R2TO
        assert_eq!(&pdu[12..16], &0u32.to_le_bytes());
        // R2TL
        assert_eq!(&pdu[16..20], &65536u32.to_le_bytes());
    }

    #[test]
    fn parse_h2cdata_round_trip() {
        const HLEN: u8 = 24;
        let payload = b"hello world!";
        let plen = u32::from(HLEN) + payload.len() as u32;
        let mut pdu_buf = Vec::new();
        let header = CommonHeader {
            pdu_type: PduType::H2CData,
            flags: H2C_FLAGS_LAST_PDU,
            hlen: HLEN,
            pdo: HLEN,
            plen,
        };
        header.write_to(&mut pdu_buf);
        pdu_buf.extend_from_slice(&0x1111u16.to_le_bytes()); // CCCID
        pdu_buf.extend_from_slice(&0x2222u16.to_le_bytes()); // TTAG
        pdu_buf.extend_from_slice(&100u32.to_le_bytes()); // DATAO
        pdu_buf.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // DATAL
        pdu_buf.extend_from_slice(&[0u8; 4]); // reserved
        pdu_buf.extend_from_slice(payload);

        let raw = RawPdu {
            header,
            body: pdu_buf[8..].to_vec(),
        };
        let h2c = parse_h2cdata(&raw).expect("parse");
        assert_eq!(h2c.cccid, 0x1111);
        assert_eq!(h2c.ttag, 0x2222);
        assert_eq!(h2c.datao, 100);
        assert_eq!(h2c.datal as usize, payload.len());
        assert!(h2c.last_pdu);
        assert_eq!(h2c.data, payload);
    }

    #[test]
    fn parse_h2cdata_rejects_length_mismatch() {
        const HLEN: u8 = 24;
        // Declare DATAL=8 but provide only 4 bytes of data.
        let plen = u32::from(HLEN) + 4;
        let mut pdu_buf = Vec::new();
        let header = CommonHeader {
            pdu_type: PduType::H2CData,
            flags: 0,
            hlen: HLEN,
            pdo: HLEN,
            plen,
        };
        header.write_to(&mut pdu_buf);
        pdu_buf.extend_from_slice(&[0u8; 2]); // CCCID
        pdu_buf.extend_from_slice(&[0u8; 2]); // TTAG
        pdu_buf.extend_from_slice(&0u32.to_le_bytes()); // DATAO
        pdu_buf.extend_from_slice(&8u32.to_le_bytes()); // DATAL = 8 (lies)
        pdu_buf.extend_from_slice(&[0u8; 4]); // reserved
        pdu_buf.extend_from_slice(&[0u8; 4]); // only 4 actual bytes

        let raw = RawPdu {
            header,
            body: pdu_buf[8..].to_vec(),
        };
        let err = parse_h2cdata(&raw).unwrap_err();
        assert!(matches!(
            err,
            PduError::H2CDataLengthMismatch {
                datal: 8,
                actual: 4
            }
        ));
    }

    #[test]
    fn sgl_data_length_reads_dptr_field() {
        let mut bytes = vec![0u8; nvme_base::SQE_SIZE];
        bytes[0] = 0x01; // Write opcode
        bytes[2] = 0x42; // CID
        // DPTR at 24..40; length at bytes 32..36 (= DPTR offset 8..12)
        bytes[32..36].copy_from_slice(&65536u32.to_le_bytes());
        let sqe = Sqe::parse(&bytes).unwrap();
        assert_eq!(sgl_data_length(&sqe), 65536);
    }

    #[test]
    fn parse_capsule_cmd_no_data_when_pdo_zero() {
        const HLEN: u8 = 72;
        let body = vec![0u8; 64];
        let header = CommonHeader {
            pdu_type: PduType::CapsuleCmd,
            flags: 0,
            hlen: HLEN,
            pdo: 0,
            plen: 8 + body.len() as u32,
        };
        let raw = RawPdu { header, body };
        let (_sqe, data) = parse_capsule_cmd(&raw).expect("parse");
        assert!(data.is_none());
    }
}
