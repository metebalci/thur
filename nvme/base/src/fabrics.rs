// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVMe over Fabrics command shapes (NVMe-oF §6).
//!
//! NVMe-oF reserves Admin opcode 0x7F ([`crate::AdminOpcode::Fabrics`])
//! for fabric-side commands. The sub-command type lives in
//! SQE.CDW10[7:0] ("FCTYPE") and selects which of the fabrics
//! commands the host is asking for:
//!
//! | FCTYPE | Command            | Notes                                  |
//! | ------ | ------------------ | -------------------------------------- |
//! | 0x00   | Property Set       | Write the controller's NVMe registers  |
//! | 0x01   | Connect            | Bind a TCP connection to (subsys, qid) |
//! | 0x04   | Property Get       | Read the controller's NVMe registers   |
//! | 0x05   | Authentication Send| DH-HMAC-CHAP / TLS-PSK key exchange    |
//! | 0x06   | Authentication Recv| ditto                                  |
//! | 0x08   | Disconnect         | Release a previously-established queue |
//!
//! Connect, Property Get/Set, and Disconnect are modeled here; the
//! Authentication Send / Receive message shapes (DH-HMAC-CHAP) live in
//! the sibling [`crate::auth`] module, driven by the controller-side
//! state machine in `nvme-tcp`.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::error::NvmeError;

/// FCTYPE byte for Admin opcode 0x7F (Fabrics) — SQE.CDW10[7:0].
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FabricsType {
    PropertySet = 0x00,
    Connect = 0x01,
    PropertyGet = 0x04,
    AuthenticationSend = 0x05,
    AuthenticationReceive = 0x06,
    Disconnect = 0x08,
}

impl FabricsType {
    pub fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x00 => Self::PropertySet,
            0x01 => Self::Connect,
            0x04 => Self::PropertyGet,
            0x05 => Self::AuthenticationSend,
            0x06 => Self::AuthenticationReceive,
            0x08 => Self::Disconnect,
            _ => return None,
        })
    }
}

/// Pull the FCTYPE out of a Fabrics SQE (NVMe-oF §6 Figure 73).
/// Fabrics SQEs put FCTYPE at byte 4 — the same byte regular SQEs use
/// for the low byte of NSID. The [`crate::Sqe`] decoder doesn't know
/// the command is Fabrics so it surfaces those bytes as `nsid`;
/// this helper does the byte-shuffle in one place so call sites
/// don't repeat the "yes really, NSID byte 0 means FCTYPE for Fabrics"
/// gymnastics.
pub fn extract_fctype(sqe: &crate::Sqe) -> Option<FabricsType> {
    FabricsType::from_u8((sqe.nsid & 0xFF) as u8)
}

/// CNTLID value the host passes in Connect.CDW10[31:16] to mean
/// "any controller". The target picks a CNTLID and returns it in
/// the Connect Response.
pub const CNTLID_ANY: u16 = 0xFFFF;

/// "Static" controller — the host is asking for a controller that
/// stays bound to one host (the alternative is "dynamic" where the
/// target may load-balance). VSA's behavior is static (one
/// controller per connection), so we don't expose the dynamic
/// alternative.
pub const CNTLID_STATIC_FLAG: u16 = 0xFFFE;

/// Connect Data structure (NVMe-oF §6.3.1.1). Fixed 1024 bytes the
/// host sends as in-capsule data on the Connect command.
///
/// Wire layout:
/// ```text
///   0..16     HOSTID    (UUID, host-chosen)
///  16..18     CNTLID    (host-requested controller ID; 0xFFFF = any)
///  18..256    reserved
/// 256..512    SUBNQN    (ASCII, NUL-padded, max 223 chars + NUL)
/// 512..768    HOSTNQN   (ASCII, NUL-padded)
/// 768..1024   reserved
/// ```
#[derive(Debug, Clone)]
pub struct ConnectData {
    pub hostid: [u8; 16],
    pub requested_cntlid: u16,
    /// SUBNQN with NUL padding stripped. The transport validates
    /// equality against the controller's own subsystem NQN; on
    /// mismatch we fail Connect with `connect_invalid_parameters`.
    pub subnqn: String,
    /// HOSTNQN with NUL padding stripped. Logged + audited, and —
    /// when TLS-PSK / DH-HMAC-CHAP is enabled — the key for per-host
    /// volume admission: it must match the TLS-negotiated host NQN and
    /// selects the admitted volume set. With auth disabled it is
    /// informational only and any host matching our SUBNQN connects.
    pub hostnqn: String,
}

impl ConnectData {
    pub const WIRE_LEN: usize = 1024;

    pub fn parse(buf: &[u8]) -> Result<Self, NvmeError> {
        if buf.len() != Self::WIRE_LEN {
            return Err(NvmeError::ConnectDataLength(buf.len()));
        }
        let mut hostid = [0u8; 16];
        hostid.copy_from_slice(&buf[0..16]);
        let requested_cntlid = u16::from_le_bytes([buf[16], buf[17]]);
        let subnqn = read_nul_padded_ascii("SUBNQN", &buf[256..512])?;
        let hostnqn = read_nul_padded_ascii("HOSTNQN", &buf[512..768])?;
        Ok(Self {
            hostid,
            requested_cntlid,
            subnqn,
            hostnqn,
        })
    }

    /// Encode back to the 1024-byte wire image. Round-trip-tested.
    pub fn to_bytes(&self) -> Result<[u8; Self::WIRE_LEN], NvmeError> {
        if self.subnqn.len() > 256 {
            return Err(NvmeError::FieldTooLong {
                field: "SUBNQN",
                got: self.subnqn.len(),
                max: 256,
            });
        }
        if self.hostnqn.len() > 256 {
            return Err(NvmeError::FieldTooLong {
                field: "HOSTNQN",
                got: self.hostnqn.len(),
                max: 256,
            });
        }
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..16].copy_from_slice(&self.hostid);
        out[16..18].copy_from_slice(&self.requested_cntlid.to_le_bytes());
        let sn = self.subnqn.as_bytes();
        out[256..256 + sn.len()].copy_from_slice(sn);
        let hn = self.hostnqn.as_bytes();
        out[512..512 + hn.len()].copy_from_slice(hn);
        Ok(out)
    }
}

/// Pack the Connect Response DW0 field (NVMe-oF §6.3.1.2):
///
/// ```text
///   bits 15:0    CNTLID assigned by the controller
///   bit  16      reserved
///   bit  17      ATR — Authentication Transaction Required. Set to
///                      require an in-band DH-HMAC-CHAP exchange before
///                      any other command is accepted.
///   bit  18      ASCR — Authentication and Secure Channel Required
///                      (TLS via the auth transaction). Not set; we use
///                      socket-level TLS-PSK for an encrypted channel.
///   bits 31:19   reserved
/// ```
///
/// The Linux host (`NVME_CONNECT_AUTHREQ_ATR = 1 << 17`) keys its auth
/// state machine off ATR — bit 16 is *not* the AUTHREQ bit, despite
/// being a natural guess.
pub fn connect_response_dw0(cntlid: u16, auth_required: bool) -> u32 {
    let mut dw0 = u32::from(cntlid);
    if auth_required {
        dw0 |= 1 << 17; // ATR
    }
    dw0
}

fn read_nul_padded_ascii(field: &'static str, buf: &[u8]) -> Result<String, NvmeError> {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let s = std::str::from_utf8(&buf[..end]).map_err(|_| NvmeError::NonAsciiField(field))?;
    Ok(s.to_string())
}

// =================== Controller register space ====================

/// NVMe controller register offsets (NVMe Base §3.1 Figure 65). Only
/// the subset a fabrics target actually responds to; PCIe-specific
/// fields (BPINFO, PMR*, doorbells) are absent.
pub mod props {
    /// Controller Capabilities. 8 bytes. Read-only.
    pub const OFFSET_CAP: u32 = 0x00;
    /// Version. 4 bytes. Read-only.
    pub const OFFSET_VS: u32 = 0x08;
    /// Interrupt Mask Set — PCIe only, returns 0 here.
    pub const OFFSET_INTMS: u32 = 0x0C;
    /// Interrupt Mask Clear — PCIe only.
    pub const OFFSET_INTMC: u32 = 0x10;
    /// Controller Configuration. 4 bytes. Host-writable.
    pub const OFFSET_CC: u32 = 0x14;
    /// Controller Status. 4 bytes. Read-only.
    pub const OFFSET_CSTS: u32 = 0x1C;
    /// NVM Subsystem Reset — write-only; ignored here.
    pub const OFFSET_NSSR: u32 = 0x20;
    /// Admin Queue Attributes — PCIe only.
    pub const OFFSET_AQA: u32 = 0x24;
    /// Admin SQ / CQ Base Address — PCIe only.
    pub const OFFSET_ASQ: u32 = 0x28;
    pub const OFFSET_ACQ: u32 = 0x30;
}

/// Property Get/Set ATTRIB field (CDW10[0:0]).
/// 0 = 4-byte property; 1 = 8-byte property (CAP is the only 8-byte).
pub const PROPERTY_ATTRIB_4: u8 = 0;
pub const PROPERTY_ATTRIB_8: u8 = 1;

/// NVMe spec version this controller claims (1.4.0). Format:
/// `MJR(16) | MNR(8) | TER(8)`.
pub const CONTROLLER_VERSION: u32 = 0x0001_0400;

/// Static `CAP` value the controller advertises.
///
/// ```text
///   bits  15:0  MQES   = 0x03FF (1024 entries, zero-based)
///   bit   16    CQR    = 0      (contiguous queues not required)
///   bits  18:17 AMS    = 0b00   (round-robin only)
///   bits  31:24 TO     = 30     (15 s ready-bit timeout, 500ms units)
///   bits  35:32 DSTRD  = 0      (doorbell stride; n/a fabrics)
///   bit   36    NSSRS  = 0      (no subsystem reset)
///   bits  44:37 CSS    = 0x01   (NVM command set supported)
///   bits  51:48 MPSMIN = 0      (4 KiB)
///   bits  55:52 MPSMAX = 0      (4 KiB)
/// ```
pub const CONTROLLER_CAP: u64 = 0x0000_0020_1E00_03FF;

/// Per-controller mutable register state. The transport shares one
/// instance via `Arc` across every connection bound to the same
/// controller; `AtomicU32` guards make the read / write paths
/// safe across concurrent fabrics commands without a lock.
#[derive(Debug)]
pub struct ControllerRegs {
    cc: AtomicU32,
    csts: AtomicU32,
}

impl ControllerRegs {
    pub fn new() -> Self {
        Self {
            cc: AtomicU32::new(0),
            csts: AtomicU32::new(0),
        }
    }

    /// Static controller capability register.
    pub fn cap(&self) -> u64 {
        CONTROLLER_CAP
    }

    pub fn vs(&self) -> u32 {
        CONTROLLER_VERSION
    }

    pub fn cc(&self) -> u32 {
        self.cc.load(Ordering::Acquire)
    }

    pub fn csts(&self) -> u32 {
        self.csts.load(Ordering::Acquire)
    }

    /// Apply a host-written `CC` value and synthesize the matching
    /// CSTS bits. Returns the resulting CSTS so callers may log it.
    ///
    /// - CC.EN (bit 0) → CSTS.RDY (bit 0) — flipped synchronously
    ///   since a software target has no real "controller startup"
    ///   work to do.
    /// - CC.SHN (bits 15:14) → CSTS.SHST (bits 3:2). Any non-zero SHN
    ///   request is acknowledged as "shutdown complete" (0b10)
    ///   immediately.
    pub fn write_cc(&self, value: u32) -> u32 {
        self.cc.store(value, Ordering::Release);
        let en = value & 0b1;
        let shn = (value >> 14) & 0b11;
        let mut csts: u32 = 0;
        if en != 0 && shn == 0 {
            csts |= 1; // RDY
        }
        if shn != 0 {
            csts |= 0b10 << 2; // SHST = complete
        }
        self.csts.store(csts, Ordering::Release);
        csts
    }

    /// Look up a property by register offset. Returns `None` for
    /// offsets we don't model (host gets Property attribute invalid).
    /// `attrib_8` reports whether the host requested the 8-byte form;
    /// CAP is the only 8-byte property and the host MUST request
    /// `ATTRIB=1` for it.
    pub fn property_get(&self, offset: u32, attrib_8: bool) -> Option<u64> {
        match offset {
            props::OFFSET_CAP if attrib_8 => Some(self.cap()),
            props::OFFSET_VS if !attrib_8 => Some(u64::from(self.vs())),
            props::OFFSET_CC if !attrib_8 => Some(u64::from(self.cc())),
            props::OFFSET_CSTS if !attrib_8 => Some(u64::from(self.csts())),
            props::OFFSET_INTMS
            | props::OFFSET_INTMC
            | props::OFFSET_NSSR
            | props::OFFSET_AQA
            | props::OFFSET_ASQ
            | props::OFFSET_ACQ
                if !attrib_8 =>
            {
                Some(0)
            }
            _ => None,
        }
    }

    /// Apply a host Property Set. Returns `Some(())` if the offset is
    /// writable, `None` if read-only / unmodeled.
    pub fn property_set(&self, offset: u32, attrib_8: bool, value: u64) -> Option<()> {
        match offset {
            props::OFFSET_CC if !attrib_8 => {
                self.write_cc(value as u32);
                Some(())
            }
            // INTMS/INTMC/NSSR/AQA/ASQ/ACQ are writable in the
            // register map but a fabrics target has no use for the
            // values. Accept silently to keep host bring-up happy.
            props::OFFSET_INTMS
            | props::OFFSET_INTMC
            | props::OFFSET_NSSR
            | props::OFFSET_AQA
            | props::OFFSET_ASQ
            | props::OFFSET_ACQ
                if !attrib_8 =>
            {
                Some(())
            }
            _ => None,
        }
    }
}

impl Default for ControllerRegs {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_data_round_trip() {
        let cd = ConnectData {
            hostid: [
                0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
                0xFF, 0x00,
            ],
            requested_cntlid: CNTLID_ANY,
            subnqn: "nqn.2025-10.com.metebalci:thurvsa".into(),
            hostnqn: "nqn.2014-08.org.nvmexpress:uuid:host-1".into(),
        };
        let wire = cd.to_bytes().expect("encode");
        let back = ConnectData::parse(&wire).expect("parse");
        assert_eq!(back.hostid, cd.hostid);
        assert_eq!(back.requested_cntlid, CNTLID_ANY);
        assert_eq!(back.subnqn, cd.subnqn);
        assert_eq!(back.hostnqn, cd.hostnqn);
    }

    #[test]
    fn connect_data_short_buffer_errors() {
        let err = ConnectData::parse(&[0u8; 512]).unwrap_err();
        assert!(matches!(err, NvmeError::ConnectDataLength(512)));
    }

    #[test]
    fn fabrics_type_from_u8() {
        assert_eq!(FabricsType::from_u8(0x01), Some(FabricsType::Connect));
        assert_eq!(FabricsType::from_u8(0x08), Some(FabricsType::Disconnect));
        assert_eq!(FabricsType::from_u8(0xFF), None);
    }

    #[test]
    fn dw0_packs_cntlid_and_authreq() {
        assert_eq!(connect_response_dw0(1, false), 1);
        // ATR is bit 17 (NVME_CONNECT_AUTHREQ_ATR), not bit 16.
        assert_eq!(connect_response_dw0(1, true), 1 | (1 << 17));
        assert_eq!(connect_response_dw0(0xABCD, false), 0xABCD);
    }

    #[test]
    fn controller_regs_cap_vs_constants() {
        let r = ControllerRegs::new();
        assert_eq!(r.cap(), CONTROLLER_CAP);
        assert_eq!(r.vs(), 0x0001_0400);
        assert_eq!(r.cc(), 0);
        assert_eq!(r.csts(), 0);
    }

    #[test]
    fn write_cc_enable_sets_ready() {
        let r = ControllerRegs::new();
        let csts = r.write_cc(0x0046_0001); // EN=1, plus IOSQES/IOCQES
        assert_eq!(csts & 1, 1, "RDY should be set when CC.EN=1");
    }

    #[test]
    fn write_cc_shutdown_sets_shst_complete() {
        let r = ControllerRegs::new();
        let csts = r.write_cc((0b01 << 14) | 1); // SHN=01, EN=1
        // SHST=0b10 (complete) at bits 3:2 → 0b1000 = 8
        assert_eq!(csts & 0b1100, 0b1000);
        // RDY clear because SHN is in progress
        assert_eq!(csts & 1, 0);
    }

    #[test]
    fn property_get_cap_requires_8_byte_attrib() {
        let r = ControllerRegs::new();
        assert_eq!(
            r.property_get(props::OFFSET_CAP, true),
            Some(CONTROLLER_CAP)
        );
        assert_eq!(r.property_get(props::OFFSET_CAP, false), None);
    }

    #[test]
    fn property_get_vs_and_csts() {
        let r = ControllerRegs::new();
        assert_eq!(r.property_get(props::OFFSET_VS, false), Some(0x0001_0400));
        assert_eq!(r.property_get(props::OFFSET_CSTS, false), Some(0));
    }

    #[test]
    fn property_set_cc_updates_csts() {
        let r = ControllerRegs::new();
        r.property_set(props::OFFSET_CC, false, 1)
            .expect("CC writable");
        assert_eq!(r.csts() & 1, 1);
    }

    #[test]
    fn property_set_unknown_offset_returns_none() {
        let r = ControllerRegs::new();
        assert!(r.property_set(0xFFFF_0000, false, 0).is_none());
    }
}
