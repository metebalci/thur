// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! iSCSI reservation-change Unit-Attention sink (issue #67).
//!
//! A [`ReservationObserver`] that turns the shared `ReservationManager`'s
//! neutral reservation changes into RESERVATIONS PREEMPTED / RESERVATIONS
//! RELEASED Unit Attentions, enqueued on the affected iSCSI initiators'
//! sessions and delivered on their next command (each product's
//! dispatcher pops a pending UA before dispatching). Registered on the
//! manager at daemon boot, so it fires regardless of which transport
//! originated the change: an NVMe-issued reservation change now reaches a
//! fenced iSCSI initiator, and an iSCSI->iSCSI change is signaled
//! proactively (it never was before).
//!
//! NVMe registrants are skipped here — the NVMe `AerReservationSink`
//! handles those. The diff + issuer-exclusion live in the manager, so
//! this sink only self-filters to iSCSI registrants, resolves each to its
//! live TSIH(s) via [`SessionManager::tsihs_for`], and enqueues the UA.

use std::sync::Arc;

use scsi_spc::reservations::{
    RegistrantId, ReservationChange, ReservationChangeKind, ReservationObserver,
};

use crate::session::SessionManager;
use crate::unit_attention::{UnitAttentionCode, UnitAttentionTracker};

/// See the module docs. Cheap to share via `Arc`; all state is behind the
/// `UnitAttentionTracker` / `SessionManager` it borrows.
pub struct IscsiReservationSink {
    ua: Arc<UnitAttentionTracker>,
    sessions: Arc<SessionManager>,
    /// PR initiator-port policy (`iscsi.reservations.initiator_port`):
    /// when set, registrant ISIDs are zeroed, so `tsihs_for` resolves the
    /// affected registrant by IQN alone.
    collapse_isid: bool,
}

impl IscsiReservationSink {
    pub fn new(
        ua: Arc<UnitAttentionTracker>,
        sessions: Arc<SessionManager>,
        collapse_isid: bool,
    ) -> Self {
        Self {
            ua,
            sessions,
            collapse_isid,
        }
    }
}

impl ReservationObserver for IscsiReservationSink {
    fn on_reservation_change(&self, changes: &[ReservationChange]) {
        for change in changes {
            // NVMe registrants are the AER sink's business.
            let RegistrantId::Iscsi { iqn, isid } = &change.affected else {
                continue;
            };
            // iSCSI addresses LUNs in a single byte; a LUN that does not
            // fit has no iSCSI-visible session to notify.
            let Ok(lun8) = u8::try_from(change.lun) else {
                continue;
            };
            // SCSI has one RESERVATIONS PREEMPTED code; the NVMe split
            // into Registration- vs Reservation-Preempted is an NVMe-wire
            // distinction, so both collapse to 0x2A/0x03 here.
            let code = match change.kind {
                ReservationChangeKind::ReservationReleased => {
                    UnitAttentionCode::RESERVATIONS_RELEASED
                }
                ReservationChangeKind::RegistrationPreempted
                | ReservationChangeKind::ReservationPreempted => {
                    UnitAttentionCode::RESERVATIONS_PREEMPTED
                }
            };
            for tsih in self
                .sessions
                .tsihs_for(iqn.as_deref(), *isid, self.collapse_isid)
            {
                self.ua.add_ua(tsih, lun8, code);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iscsi(iqn: &str, isid: [u8; 6]) -> RegistrantId {
        RegistrantId::iscsi(Some(iqn.to_string()), isid)
    }

    #[test]
    fn preempt_enqueues_ua_on_affected_iscsi_session() {
        let ua = Arc::new(UnitAttentionTracker::new());
        let sessions = Arc::new(SessionManager::new());
        let isid = [1u8; 6];
        let tsih = sessions.create_session(isid);
        sessions.set_initiator_iqn(tsih, Some("iqn.test:a".into()));
        let sink = IscsiReservationSink::new(Arc::clone(&ua), Arc::clone(&sessions), false);

        sink.on_reservation_change(&[ReservationChange {
            lun: 3,
            affected: iscsi("iqn.test:a", isid),
            kind: ReservationChangeKind::ReservationPreempted,
        }]);
        assert_eq!(
            ua.check_and_pop_ua(tsih, 3),
            Some(UnitAttentionCode::RESERVATIONS_PREEMPTED)
        );
    }

    #[test]
    fn release_maps_to_released_code() {
        let ua = Arc::new(UnitAttentionTracker::new());
        let sessions = Arc::new(SessionManager::new());
        let isid = [2u8; 6];
        let tsih = sessions.create_session(isid);
        sessions.set_initiator_iqn(tsih, Some("iqn.test:b".into()));
        let sink = IscsiReservationSink::new(Arc::clone(&ua), Arc::clone(&sessions), false);

        sink.on_reservation_change(&[ReservationChange {
            lun: 0,
            affected: iscsi("iqn.test:b", isid),
            kind: ReservationChangeKind::ReservationReleased,
        }]);
        assert_eq!(
            ua.check_and_pop_ua(tsih, 0),
            Some(UnitAttentionCode::RESERVATIONS_RELEASED)
        );
    }

    #[test]
    fn nvme_registrant_is_skipped() {
        let ua = Arc::new(UnitAttentionTracker::new());
        let sessions = Arc::new(SessionManager::new());
        let isid = [3u8; 6];
        let tsih = sessions.create_session(isid);
        sessions.set_initiator_iqn(tsih, Some("iqn.test:c".into()));
        let sink = IscsiReservationSink::new(Arc::clone(&ua), Arc::clone(&sessions), false);

        sink.on_reservation_change(&[ReservationChange {
            lun: 0,
            affected: RegistrantId::nvme([0xAB; 16]),
            kind: ReservationChangeKind::ReservationPreempted,
        }]);
        assert!(!ua.has_pending_ua(tsih, 0));
    }

    #[test]
    fn no_live_session_is_a_noop() {
        let ua = Arc::new(UnitAttentionTracker::new());
        let sessions = Arc::new(SessionManager::new());
        let sink = IscsiReservationSink::new(Arc::clone(&ua), Arc::clone(&sessions), false);
        // No session exists for this registrant — must not panic, no UA.
        sink.on_reservation_change(&[ReservationChange {
            lun: 0,
            affected: iscsi("iqn.test:gone", [9u8; 6]),
            kind: ReservationChangeKind::ReservationReleased,
        }]);
        // Nothing to assert beyond "did not panic / enqueue"; pop a
        // never-keyed slot to confirm emptiness.
        assert!(!ua.has_pending_ua(1, 0));
    }

    #[test]
    fn collapse_mode_matches_by_iqn_with_zeroed_isid() {
        let ua = Arc::new(UnitAttentionTracker::new());
        let sessions = Arc::new(SessionManager::new());
        // Session has a real wire ISID; the registrant (collapse mode)
        // carries a zeroed ISID. IQN-only match must still find it.
        let tsih = sessions.create_session([0x5A; 6]);
        sessions.set_initiator_iqn(tsih, Some("iqn.test:d".into()));
        let sink = IscsiReservationSink::new(Arc::clone(&ua), Arc::clone(&sessions), true);

        sink.on_reservation_change(&[ReservationChange {
            lun: 1,
            affected: iscsi("iqn.test:d", [0u8; 6]),
            kind: ReservationChangeKind::ReservationPreempted,
        }]);
        assert_eq!(
            ua.check_and_pop_ua(tsih, 1),
            Some(UnitAttentionCode::RESERVATIONS_PREEMPTED)
        );
    }
}
