// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Persistent Reservation primitive types — SPC-4 §6.16
//! (PERSISTENT RESERVE IN / OUT).
//!
//! This module owns the spec enums (scope, type, service action)
//! plus a tiny `ReservationKey` newtype that wraps the 8-byte key
//! initiators register. Actual state-machine bookkeeping
//! (registrations + reservation enforcement) lives in the
//! per-product dispatcher: today only thurvsa implements it
//! (`vsa/daemon/src/scsi/reservations.rs`); thurvtl's tape
//! surface doesn't surface PR yet.
//!
//! Keeping the enums here means the SPC-4 wire-level layout is
//! single-sourced — when thurvsa or any future product grows
//! its own reservation manager, it pulls the same `Type` discriminants
//! and `RESERVATION_KEY_LEN` constant rather than redefining them.

/// SCSI reservation key length (SPC-4 §6.16.2 — always 8 bytes).
pub const RESERVATION_KEY_LEN: usize = 8;

/// Reservation key. Newtype around an 8-byte BLOCK so callers
/// can't accidentally compare two keys via `Vec<u8>` slicing.
/// PERSISTENT RESERVE OUT REGISTER carries the 8-byte
/// `SERVICE ACTION RESERVATION KEY` field; this wraps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReservationKey(pub [u8; RESERVATION_KEY_LEN]);

impl ReservationKey {
    /// All-zero key — special-cased by SPC-4 to mean "no key" in
    /// many places (REGISTER with key=0 unregisters; a held
    /// reservation key=0 means no reservation held).
    pub const ZERO: Self = Self([0u8; RESERVATION_KEY_LEN]);

    /// Construct from the 8 bytes the wire carries (big-endian
    /// interpretation is host-defined; we treat as opaque).
    pub const fn new(bytes: [u8; RESERVATION_KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// Return the underlying 8-byte slice.
    pub const fn as_bytes(&self) -> &[u8; RESERVATION_KEY_LEN] {
        &self.0
    }

    /// True if this is the all-zero "no key" sentinel.
    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; RESERVATION_KEY_LEN]
    }
}

/// PERSISTENT RESERVE IN service actions (SPC-4 §6.16.2 Table 218).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PrInServiceAction {
    ReadKeys = 0x00,
    ReadReservation = 0x01,
    ReportCapabilities = 0x02,
    ReadFullStatus = 0x03,
}

impl PrInServiceAction {
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x00 => Some(Self::ReadKeys),
            0x01 => Some(Self::ReadReservation),
            0x02 => Some(Self::ReportCapabilities),
            0x03 => Some(Self::ReadFullStatus),
            _ => None,
        }
    }
}

/// PERSISTENT RESERVE OUT service actions (SPC-4 §6.16.3 Table 220).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PrOutServiceAction {
    Register = 0x00,
    Reserve = 0x01,
    Release = 0x02,
    Clear = 0x03,
    Preempt = 0x04,
    PreemptAndAbort = 0x05,
    RegisterAndIgnoreExistingKey = 0x06,
    RegisterAndMove = 0x07,
}

impl PrOutServiceAction {
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x00 => Some(Self::Register),
            0x01 => Some(Self::Reserve),
            0x02 => Some(Self::Release),
            0x03 => Some(Self::Clear),
            0x04 => Some(Self::Preempt),
            0x05 => Some(Self::PreemptAndAbort),
            0x06 => Some(Self::RegisterAndIgnoreExistingKey),
            0x07 => Some(Self::RegisterAndMove),
            _ => None,
        }
    }
}

/// PERSISTENT RESERVE scope (SPC-4 §6.16.3.2). Only LU_SCOPE
/// (whole logical unit) is defined / used today; element scope
/// (changers) and LBA-range scope are obsolete in SPC-4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ReservationScope {
    LogicalUnit = 0x00,
}

impl ReservationScope {
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x00 => Some(Self::LogicalUnit),
            _ => None,
        }
    }
}

/// PERSISTENT RESERVE type field (SPC-4 §6.16.3.3 Table 221). The
/// six "current" types — `WriteExclusive` through
/// `ExclusiveAccessAllRegistrants` — are what thurvsa's REPORT
/// CAPABILITIES advertises as the SBC-3 baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ReservationType {
    /// 0x01 — Write Exclusive: only the holder can write; reads
    /// are open to every initiator.
    WriteExclusive = 0x01,
    /// 0x03 — Exclusive Access: only the holder can read or write.
    ExclusiveAccess = 0x03,
    /// 0x05 — Write Exclusive, Registrants Only.
    WriteExclusiveRegistrantsOnly = 0x05,
    /// 0x06 — Exclusive Access, Registrants Only.
    ExclusiveAccessRegistrantsOnly = 0x06,
    /// 0x07 — Write Exclusive, All Registrants.
    WriteExclusiveAllRegistrants = 0x07,
    /// 0x08 — Exclusive Access, All Registrants.
    ExclusiveAccessAllRegistrants = 0x08,
}

impl ReservationType {
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x01 => Some(Self::WriteExclusive),
            0x03 => Some(Self::ExclusiveAccess),
            0x05 => Some(Self::WriteExclusiveRegistrantsOnly),
            0x06 => Some(Self::ExclusiveAccessRegistrantsOnly),
            0x07 => Some(Self::WriteExclusiveAllRegistrants),
            0x08 => Some(Self::ExclusiveAccessAllRegistrants),
            _ => None,
        }
    }

    /// Wire byte (mirror of the discriminant — exposed as a method
    /// so call sites read intent rather than `as u8`).
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// All-registrants types (WR_EX_AR / EX_AC_AR) survive holder
    /// disappearance as long as another registrant remains; for
    /// non-AR types the reservation is released when the holder
    /// unregisters (SPC-4 §5.13.4.2). Consumed by the shared
    /// [`crate::reservations::ReservationManager`] state machine.
    pub const fn is_all_registrants(self) -> bool {
        matches!(
            self,
            Self::WriteExclusiveAllRegistrants | Self::ExclusiveAccessAllRegistrants
        )
    }

    /// Bitmask of supported types for REPORT CAPABILITIES TYPE_MASK
    /// bytes (SPC-4 §6.16.2.4 Table 219). Each bit position
    /// corresponds to a `ReservationType` value; the helper sets the
    /// matching bit in the 16-bit mask. thurvsa advertises every
    /// type today (mask `0xEA, 0x01`).
    pub const fn type_mask_bit(self) -> u16 {
        // SPC-4 layout: byte 4 of the response carries types
        // 0x01..0x08 (bits 1..7 + 1 carry), byte 5 carries 0x09+.
        // Easier model: shift the wire byte directly.
        1u16 << (self as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_zero_round_trip() {
        let k = ReservationKey::ZERO;
        assert!(k.is_zero());
        assert_eq!(k.as_bytes(), &[0u8; RESERVATION_KEY_LEN]);
    }

    #[test]
    fn key_nonzero() {
        let k = ReservationKey::new([1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(!k.is_zero());
    }

    #[test]
    fn pr_in_service_action_decode() {
        assert_eq!(
            PrInServiceAction::from_u8(0x00),
            Some(PrInServiceAction::ReadKeys)
        );
        assert_eq!(
            PrInServiceAction::from_u8(0x03),
            Some(PrInServiceAction::ReadFullStatus)
        );
        assert_eq!(PrInServiceAction::from_u8(0x04), None);
    }

    #[test]
    fn pr_out_service_action_decode() {
        assert_eq!(
            PrOutServiceAction::from_u8(0x06),
            Some(PrOutServiceAction::RegisterAndIgnoreExistingKey)
        );
        assert_eq!(PrOutServiceAction::from_u8(0x08), None);
    }

    #[test]
    fn reservation_type_round_trip() {
        for value in [0x01u8, 0x03, 0x05, 0x06, 0x07, 0x08] {
            let t = ReservationType::from_u8(value).unwrap();
            assert_eq!(t.as_u8(), value);
        }
        assert_eq!(ReservationType::from_u8(0x02), None);
        assert_eq!(ReservationType::from_u8(0x04), None);
    }

    #[test]
    fn thurvsa_advertised_types_match_known_mask() {
        // thurvsa today advertises every SBC-3 type — REPORT
        // CAPABILITIES bytes 4..6 are `0xEA, 0x01`. The 16-bit
        // mask we OR per supported type should land on the same
        // value (bits 1, 3, 5, 6, 7, 8).
        let mut mask: u16 = 0;
        for t in [
            ReservationType::WriteExclusive,
            ReservationType::ExclusiveAccess,
            ReservationType::WriteExclusiveRegistrantsOnly,
            ReservationType::ExclusiveAccessRegistrantsOnly,
            ReservationType::WriteExclusiveAllRegistrants,
            ReservationType::ExclusiveAccessAllRegistrants,
        ] {
            mask |= t.type_mask_bit();
        }
        // Bits set: 1, 3, 5, 6, 7, 8.
        assert_eq!(mask & (1 << 1), 1 << 1);
        assert_eq!(mask & (1 << 3), 1 << 3);
        assert_eq!(mask & (1 << 5), 1 << 5);
        assert_eq!(mask & (1 << 6), 1 << 6);
        assert_eq!(mask & (1 << 7), 1 << 7);
        assert_eq!(mask & (1 << 8), 1 << 8);
        assert_eq!(mask & (1 << 0), 0);
        assert_eq!(mask & (1 << 2), 0);
        assert_eq!(mask & (1 << 4), 0);
    }

    #[test]
    fn reservation_scope_decode() {
        assert_eq!(
            ReservationScope::from_u8(0x00),
            Some(ReservationScope::LogicalUnit)
        );
        assert_eq!(ReservationScope::from_u8(0x01), None);
    }
}
