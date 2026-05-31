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
//! `thurvsad` — the operator picks one via `transport:
//! iscsi | nvmetcp` in `thurvsa.yaml`; the two are mutually
//! exclusive so the iSCSI / NVMe-TCP listeners don't fight over
//! port 3260.
//!
//! # Current status (session 1 scaffold)
//!
//! This crate ships the wire-shape types every subsequent session
//! plugs into:
//!
//! - [`pdu::PduType`] — every PDU type byte defined in the spec.
//! - [`pdu::CommonHeader`] — fixed 8-byte PDU header (PDU type +
//!   flags + HPDA + PDO + PLEN).
//! - [`pdu::ICReq`] / [`pdu::ICResp`] — Initialize Connection
//!   handshake payload structures.
//! - [`ServerConfig`] / [`run`] — server entry stub that binds the
//!   TCP listen address and accepts connections; the per-connection
//!   loop currently logs "not yet wired" and closes. Wired in
//!   `thurvsad`'s boot path behind the `transport: nvmetcp`
//!   selector so the YAML knob round-trips end-to-end.
//!
//! Deferred to follow-up sessions:
//!
//! - Full PDU codec (read + write loop with HPDA / PDO / PLEN
//!   honored, header-digest + data-digest fields).
//! - ICReq / ICResp handshake.
//! - Connect handler (NVMe Admin Fabrics command 0x7F sub-type 0x01)
//!   — first SQE on a new connection, carries the host's HostNQN
//!   and the target's SubsystemNQN.
//! - R2T flow control + H2CData collection.
//! - TLS 1.3 PSK auth (recommended MVP per NVMe-oF §8.13.2);
//!   DH-HMAC-CHAP is the alternative for non-TLS deployments and
//!   lands later.

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
