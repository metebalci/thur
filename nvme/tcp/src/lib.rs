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
