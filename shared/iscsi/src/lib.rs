// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cross-product iSCSI primitives.
//!
//! Lifted from `thurvtld::iscsi` so thurvsad (block product)
//! and thurvtld (tape product) can share the same building blocks
//! without one depending on the other.
//!
//! - **Phase 1** (shipped 2026-05-09): CHAP auth, per-(TSIH, LUN)
//!   Unit Attention queue, iSCSI session manager
//!   (TSIH / CmdSN / StatSN), generic SCSI sense surface.
//! - **Phase 2** (shipped 2026-05-09): transport split — PDU framing
//!   ([`transport::Pdu`], [`transport::read_pdu`] /
//!   [`transport::write_pdu`]), login phase
//!   ([`transport::handle_login_phase`]), R2T loop
//!   ([`transport::collect_write_data`]), connection lifecycle
//!   ([`transport::serve_connection`] / [`transport::run`]), and the
//!   product-agnostic [`ScsiHandler`] trait the transport dispatches
//!   through.

pub mod auth;
pub mod error;
pub mod handler;
#[cfg(feature = "http")]
pub mod http;
pub mod metrics;
pub mod sense;
pub mod session;
pub mod transport;
pub mod unit_attention;

pub use error::IscsiError;
pub use handler::{ScsiHandler, ScsiRequest, ScsiResponse, ScsiStatus};
