// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVMe over Fabrics TCP transport (NVMe-oF TCP Transport Spec).
//!
//! Sibling of `shared-iscsi` — same role (frame the wire protocol,
//! run the connection lifecycle, dispatch decoded commands through
//! a product-agnostic handler trait), different wire.
//!
//! Layered above [`nvme_base`] (SQE / CQE / Identify) and
//! [`nvme_nvm`] (the NVM Command Set handler trait this transport
//! dispatches into). Co-resident with the iSCSI transport in
//! `thurvsad`: the operator lists one or both in `transports:` in
//! `thurvsa.yaml` (default `[iscsi]`). The two bind concurrently —
//! NVMe/TCP defaults to `0.0.0.0:4420`, iSCSI to `:3260`, so the
//! listeners don't clash (issue #66).
//!
//! # Modules
//!
//! - [`pdu`] — PDU codec: every PDU type byte, the fixed 8-byte
//!   common header, ICReq / ICResp, CapsuleCmd / CapsuleResp,
//!   H2CData / C2HData, R2T, TermReq, plus the CRC32C header- and
//!   data-digest apply / verify helpers (issue #78).
//! - [`server`] — the per-connection state machine ([`run`] /
//!   [`ServerConfig`]): ICReq -> ICResp handshake, Connect with SUBNQN
//!   admission, Property Get / Set, Disconnect, the command loop with
//!   R2T write flow + fused Compare+Write pairing, and C2HTermReq on
//!   protocol violations.
//! - [`auth`] — DH-HMAC-CHAP authentication (Authentication Send /
//!   Receive) over the negotiated FFDHE group.
//! - [`tls`] / [`identity`] — TLS 1.3 PSK channel and the per-host PSK
//!   identity store (`nvmetcp-psks.json`).
//!
//! The transport's behavioral model, opcode -> PageCache mapping, NQN
//! handling, reservation-notification path, and TLS-PSK / DH-HMAC-CHAP
//! design are in `docs/NVMETCP.md`.

#![forbid(unsafe_code)]

pub mod auth;
mod ffdhe;
pub mod identity;
pub mod pdu;
pub mod server;
pub mod tls;

pub use server::{LoginAuditEvent, LoginAuditSink, NoopLoginAudit, ServerConfig, run};

/// Verify the trailing 4-byte little-endian CRC-32 carried by the
/// `NVMeTLSkey-...` and `DHHC-1:...` secret formats. The kernel encodes
/// both as `base64(key_bytes || crc32_le(key_bytes))`; this is the
/// shared CRC-tail core of [`tls::parse_interchange_key`] and
/// [`auth::parse_dhchap_secret`] (issue #70).
///
/// `decoded` is the base64-decoded body; the caller is responsible for
/// having validated its overall length first (the two formats disagree
/// on the legal key lengths and surface distinct length errors). On a
/// CRC match the key bytes (CRC stripped) are returned; on mismatch —
/// or a body shorter than the 4-byte CRC — `None`.
pub(crate) fn split_verify_crc_tail(decoded: &[u8]) -> Option<&[u8]> {
    if decoded.len() < 4 {
        return None;
    }
    let (key, crc_bytes) = decoded.split_at(decoded.len() - 4);
    let stored = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
    (crc32fast::hash(key) == stored).then_some(key)
}
