// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

use thiserror::Error;

/// Errors surfaced by the `shared-iscsi` primitives. Kept narrow on
/// purpose — only the cases the lifted modules (auth, session) need to
/// raise. Both consuming products (`core-mediachanger`, `core-block`) define
/// `From<IscsiError>` for their own error type so the existing `?`
/// propagation in handlers is unaffected.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IscsiError {
    /// Generic invariant violation with a static message — mostly
    /// poisoned-mutex paths and immutability checks (e.g. trying to
    /// rebind a session's partition after login).
    #[error("invalid operation: {0}")]
    InvalidOp(&'static str),

    /// The TSIH does not name a live session in the manager. Surfaces
    /// as ILLEGAL REQUEST at the SCSI layer in the consumer.
    #[error("invalid session TSIH: {0}")]
    InvalidSession(u16),

    /// CHAP verification failed: unknown user or target password not
    /// configured for the mutual-auth path. Carried message is
    /// human-readable and never reaches the host (login is rejected
    /// with the spec-defined CHAP failure code).
    #[error("authentication failed: {0}")]
    AuthFailed(String),

    /// Invalid CHAP configuration — raised by
    /// [`crate::auth::parse_chap_algorithms`] when the operator lists
    /// an unknown algorithm name in `iscsi.auth.allowed_algorithms`.
    #[error("invalid CHAP configuration: {0}")]
    InvalidConfig(String),
}

pub type Result<T> = std::result::Result<T, IscsiError>;
