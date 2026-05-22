// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Transport-agnostic SCSI command shapes — request, response,
//! status. Lifted out of `shared/iscsi/src/handler.rs` (where the
//! `ScsiHandler` trait lives) so the iSCSI transport and both
//! products' SCSI dispatchers share one set of structs without the
//! transport-only `ScsiHandler` trait dragging in `async_trait`.
//!
//! These mirror the SAM-5 model: a request is one CDB plus its
//! Data-Out payload (already drained by the transport, including
//! R2T loops), and a response is `(status, sense, data_in)` ready
//! to be wrapped into the transport's response framing. Sense is
//! the structured [`crate::sense::SenseData`] value — the transport
//! serializes via [`SenseData::to_bytes`] before stuffing into the
//! SCSI Response data segment. Tape callers that already produce
//! pre-encoded fixed-format bytes (via [`crate::sense::SenseDataBuilder`])
//! lift them back to structured form via
//! [`crate::sense::SenseData::from_wire_bytes`].

/// SCSI status code (SAM-5 Table 47, subset). `Good` (0x00) and
/// `CheckCondition` (0x02) cover most surfaces; thurvsa's PERSISTENT
/// RESERVE enforcement raises `ReservationConflict` (0x18) when an
/// I_T nexus tries to access a LUN it isn't a registrant of.
/// `Busy` / `TaskSetFull` aren't surfaced today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScsiStatus {
    Good,
    CheckCondition,
    ReservationConflict,
}

impl ScsiStatus {
    /// Encode for the SCSI Response BHS byte 3 / Data-In with S=1
    /// status field.
    pub const fn code(self) -> u8 {
        match self {
            ScsiStatus::Good => 0x00,
            ScsiStatus::CheckCondition => 0x02,
            ScsiStatus::ReservationConflict => 0x18,
        }
    }
}

/// One SCSI command end-to-end: the transport has already handled
/// the iSCSI framing and (for write commands) drained Data-Out.
/// Borrowed lifetimes tie back to the per-PDU buffers — handlers
/// must finish reading before the next `read_pdu` reuses them.
pub struct ScsiRequest<'a> {
    /// Target session identifier allocated at login.
    pub tsih: u16,
    /// Connection identifier within the session (0 for the leading /
    /// only connection in single-connection sessions).
    pub cid: u16,
    /// SAM-5 LUN, decoded into a u64 so flat-space (LUN 256+)
    /// callers don't truncate. The transport extracts the iSCSI
    /// PDU's 8-byte LUN field and decodes; today every product
    /// uses single-level peripheral-device addressing (LUNs 0..255)
    /// so the value fits in the low byte. Handlers that want a
    /// `u8` should `req.lun as u8` after the upper bytes are
    /// guaranteed-zero.
    pub lun: u64,
    /// 6 / 10 / 12 / 16-byte Command Descriptor Block. Sliced from
    /// the SCSI Command BHS bytes 32..48 — handlers should not
    /// assume any particular length, just read the bytes the opcode
    /// expects.
    pub cdb: &'a [u8],
    /// Concatenated Data-Out payload (immediate data + every
    /// solicited / unsolicited Data-Out PDU). Empty for read-side
    /// or no-data commands.
    pub data_out: &'a [u8],
    /// `ExpectedDataTransferLength` from the SCSI Command BHS — how
    /// many bytes of Data-In the initiator is willing to accept.
    /// Handlers should truncate their response to this size (the
    /// transport surfaces residual under/overflow on its own).
    pub data_in_max: usize,
    /// IQN the initiator advertised at login. `None` until the login
    /// phase captures `InitiatorName=`.
    pub initiator_iqn: Option<&'a str>,
    /// Peer socket address ("ip:port"). Pass-through for audit logging.
    pub peer: &'a str,
    /// Logical partition the session was bound to (CHAP user →
    /// partition mapping). `None` = no fence (legacy unpartitioned
    /// access; thurvsa never sets this).
    pub session_partition: Option<&'a str>,
}

/// Result of one dispatched SCSI command, ready for the iSCSI
/// transport to wrap into a Data-In + status / SCSI Response PDU
/// pair.
///
/// `sense` is structured (`Option<SenseData>`) so test assertions
/// can compare the spec triple directly
/// (`assert_eq!(r.sense, Some(SenseData::INVALID_OPCODE));`); the
/// transport calls [`crate::sense::SenseData::to_bytes`] before
/// length-prefixing per RFC 3720 §10.4.6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScsiResponse {
    pub status: ScsiStatus,
    /// Structured sense. `None` for `Good` (no CHECK CONDITION).
    pub sense: Option<crate::sense::SenseData>,
    /// Data-In payload (READ-side bytes). Empty for write-only or
    /// no-data commands. Handlers may exceed `data_in_max`; the
    /// transport truncates and reports residual overflow / underflow.
    pub data_in: Vec<u8>,
}

impl ScsiResponse {
    /// `GOOD` with the given Data-In payload (may be empty).
    pub fn good(data_in: Vec<u8>) -> Self {
        Self {
            status: ScsiStatus::Good,
            sense: None,
            data_in,
        }
    }

    /// `CHECK CONDITION` from a structured
    /// [`crate::sense::SenseData`] value. The transport serializes
    /// at PDU-wrap time using the format the value carries
    /// (descriptor or fixed).
    pub fn check(sense: crate::sense::SenseData) -> Self {
        Self {
            status: ScsiStatus::CheckCondition,
            sense: Some(sense),
            data_in: Vec::new(),
        }
    }

    /// `CHECK CONDITION` from pre-encoded sense bytes — the legacy
    /// path for tape callers that go through
    /// [`crate::sense::SenseDataBuilder::build`]. Decodes back into
    /// structured form via [`crate::sense::SenseData::from_wire_bytes`];
    /// returns `None` (no sense) for empty input or unrecognized
    /// response codes (the latter is a caller bug).
    pub fn check_condition(sense: Vec<u8>) -> Self {
        let structured = crate::sense::SenseData::from_wire_bytes(&sense);
        Self {
            status: ScsiStatus::CheckCondition,
            sense: structured,
            data_in: Vec::new(),
        }
    }

    /// `RESERVATION CONFLICT` (SAM-5 status 0x18). Per SPC-4 §6.16
    /// no sense data accompanies this status; the initiator
    /// recognizes the situation from the status code alone.
    pub fn reservation_conflict() -> Self {
        Self {
            status: ScsiStatus::ReservationConflict,
            sense: None,
            data_in: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sense::{ASC_INVALID_COMMAND, SenseData, SenseDataBuilder, SenseKey};

    #[test]
    fn status_code_round_trip() {
        assert_eq!(ScsiStatus::Good.code(), 0x00);
        assert_eq!(ScsiStatus::CheckCondition.code(), 0x02);
        assert_eq!(ScsiStatus::ReservationConflict.code(), 0x18);
    }

    #[test]
    fn good_response_has_no_sense() {
        let r = ScsiResponse::good(vec![1, 2, 3]);
        assert_eq!(r.status, ScsiStatus::Good);
        assert!(r.sense.is_none());
        assert_eq!(r.data_in, vec![1, 2, 3]);
    }

    #[test]
    fn check_carries_structured_sense() {
        let r = ScsiResponse::check(SenseData::new(SenseKey::IllegalRequest, 0x20, 0x00));
        assert_eq!(r.status, ScsiStatus::CheckCondition);
        assert_eq!(
            r.sense,
            Some(SenseData::new(SenseKey::IllegalRequest, 0x20, 0x00))
        );
        assert!(r.data_in.is_empty());
    }

    #[test]
    fn check_condition_decodes_fixed_bytes_to_structured() {
        let raw = SenseDataBuilder::new(SenseKey::IllegalRequest, ASC_INVALID_COMMAND).build();
        let r = ScsiResponse::check_condition(raw);
        assert_eq!(r.status, ScsiStatus::CheckCondition);
        let s = r.sense.expect("decoded structured sense");
        assert_eq!(s.key, SenseKey::IllegalRequest);
        assert_eq!(s.asc, 0x20);
        assert_eq!(s.ascq, 0x00);
    }

    #[test]
    fn reservation_conflict_carries_no_sense() {
        let r = ScsiResponse::reservation_conflict();
        assert_eq!(r.status, ScsiStatus::ReservationConflict);
        assert!(r.sense.is_none());
        assert!(r.data_in.is_empty());
    }
}
