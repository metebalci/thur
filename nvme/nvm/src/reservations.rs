// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! NVMe reservation command adapter.
//!
//! The block-side counterpart of `scsi_sbc::reservations`: it parses
//! the NVMe Reservation Register / Acquire / Release / Report wire
//! fields (via `nvme_base::reservation`), drives the shared
//! `scsi_spc::reservations::ReservationManager` semantic ops keyed by
//! the host's 128-bit HOSTID, and maps the neutral outcome back onto
//! a [`NvmeResponse`]. The state machine itself lives in `scsi-spc`,
//! so the iSCSI and NVMe reservation surfaces can't drift.
//!
//! NSID maps onto the manager's per-entity `lun` key as `nsid - 1`
//! (see [`nsid_to_lun`]) — the same stable LUN the SCSI/SBC path and
//! the PTPL persistence resolver use, so a persisted reservation
//! round-trips and the two transports key one volume identically.

use nvme_base::reservation as wire;
use nvme_base::{Cqe, Sqe, StatusField};
use scsi_spc::pr::ReservationType;
use scsi_spc::reservations::{PrOutOutcome, RegistrantId, ReservationManager};

use crate::aer::{ReservationEvent, ResvAction, diff_reservation_events};
use crate::handler::NvmeResponse;

/// The shared `ReservationManager` keys per-entity state by the stable
/// LUN — the same key the SCSI / SBC path and the persistence
/// `EntityResolver` (volume UUID -> LUN) use. NVMe addresses namespaces
/// as `nsid = lun + 1`, so map back to the LUN here. Keeping both
/// transports on the same key is what makes PTPL persistence resolve
/// correctly (issue #57) and lets a future dual-transport export key one
/// volume identically over iSCSI and NVMe.
pub(crate) fn nsid_to_lun(nsid: u32) -> u64 {
    u64::from(nsid.saturating_sub(1))
}

/// Map a neutral PROUT-style outcome onto an NVMe completion.
fn map_outcome(cid: u16, outcome: PrOutOutcome) -> NvmeResponse {
    let status = match outcome {
        PrOutOutcome::Good => return NvmeResponse::just(Cqe::success(cid, 0, 0, 0)),
        PrOutOutcome::ReservationConflict => StatusField::reservation_conflict(),
        PrOutOutcome::InvalidFieldInCdb | PrOutOutcome::InvalidFieldInParameterList => {
            StatusField::invalid_field()
        }
        // The semantic ops never report LuNotSupported (the dispatcher
        // resolves the NSID before calling us), but map it honestly.
        PrOutOutcome::LuNotSupported => StatusField::invalid_namespace(),
        // PTPL persist-before-ack failed — do not complete successfully.
        PrOutOutcome::PersistFailed => StatusField::internal_error(),
    };
    NvmeResponse::just(Cqe::failure(cid, 0, 0, status))
}

fn fail(cid: u16, status: StatusField) -> NvmeResponse {
    NvmeResponse::just(Cqe::failure(cid, 0, 0, status))
}

/// Reservation Register (0x0D) — RREGA Register / Unregister /
/// Replace, with IEKEY + CPTPL.
pub fn reservation_register(
    mgr: &ReservationManager,
    nsid: u32,
    host_id: [u8; 16],
    sqe: &Sqe,
    data_out: Option<&[u8]>,
) -> (NvmeResponse, Vec<ReservationEvent>) {
    let cid = sqe.cid;
    let lun = nsid_to_lun(nsid);
    let id = RegistrantId::nvme(host_id);

    // CPTPL -> per-LU APTPL bit (issue #57): no-change leaves it,
    // clear/set apply false/true. A "set" is honored only when the
    // manager can actually persist; otherwise reject it (mirror of the
    // SCSI APTPL=1 reject so RESCAP bit 0 and behavior stay consistent).
    let aptpl = match wire::cptpl(sqe.cdw10) {
        wire::CPTPL_NO_CHANGE => None,
        wire::CPTPL_CLEAR => Some(false),
        wire::CPTPL_PERSIST if mgr.ptpl_capable() => Some(true),
        wire::CPTPL_PERSIST => return (fail(cid, StatusField::invalid_field()), Vec::new()),
        _ => return (fail(cid, StatusField::invalid_field()), Vec::new()),
    };
    let Some((crkey, nrkey)) = data_out.and_then(wire::parse_register_keys) else {
        return (fail(cid, StatusField::invalid_field()), Vec::new());
    };
    let iekey = wire::iekey(sqe.cdw10);
    let (outcome, events) = match wire::action(sqe.cdw10) {
        // Register and Replace both set the registration to NRKEY
        // after the CRKEY check (skipped when IEKEY=1). Neither fences
        // another host, so there is nothing to notify.
        wire::RREGA_REGISTER | wire::RREGA_REPLACE => (
            mgr.register(lun, &id, crkey, nrkey, iekey, aptpl),
            Vec::new(),
        ),
        // Unregister: NRKEY ignored, registration removed. If the
        // departing host held a (non-all-registrants) reservation it is
        // released, which fans a Reservation Released to the survivors.
        wire::RREGA_UNREGISTER => {
            let pre = mgr.snapshot(lun);
            let outcome = mgr.register(lun, &id, crkey, 0, iekey, aptpl);
            let events =
                reservation_events(mgr, ResvAction::Unregister, outcome, &pre, host_id, nsid);
            (outcome, events)
        }
        _ => return (fail(cid, StatusField::invalid_field()), Vec::new()),
    };
    (map_outcome(cid, outcome), events)
}

/// Diff the post-op snapshot against `pre` and derive notifications,
/// but only for a successful (`Good`) mutation. The post-op snapshot is
/// taken here so callers don't repeat the conditional.
fn reservation_events(
    mgr: &ReservationManager,
    action: ResvAction,
    outcome: PrOutOutcome,
    pre: &scsi_spc::reservations::ReservationSnapshot,
    host_id: [u8; 16],
    nsid: u32,
) -> Vec<ReservationEvent> {
    if !matches!(outcome, PrOutOutcome::Good) {
        return Vec::new();
    }
    let post = mgr.snapshot(nsid_to_lun(nsid));
    diff_reservation_events(action, pre, &post, host_id, nsid)
}

/// Reservation Acquire (0x11) — RACQA Acquire / Preempt /
/// Preempt-and-Abort. IEKEY is ignored on Acquire/Release (CRKEY is
/// always validated, 1.2.1 model).
pub fn reservation_acquire(
    mgr: &ReservationManager,
    nsid: u32,
    host_id: [u8; 16],
    sqe: &Sqe,
    data_out: Option<&[u8]>,
) -> (NvmeResponse, Vec<ReservationEvent>) {
    let cid = sqe.cid;
    let lun = nsid_to_lun(nsid);
    let id = RegistrantId::nvme(host_id);

    let Some(scsi_type) = wire::nvme_rtype_to_scsi_byte(wire::rtype(sqe.cdw10)) else {
        return (fail(cid, StatusField::invalid_field()), Vec::new());
    };
    let Some((crkey, prkey)) = data_out.and_then(wire::parse_acquire_keys) else {
        return (fail(cid, StatusField::invalid_field()), Vec::new());
    };
    let (outcome, events) = match wire::action(sqe.cdw10) {
        // Acquire only takes a free reservation — it never fences an
        // existing registrant, so there is nothing to notify.
        wire::RACQA_ACQUIRE => (mgr.reserve(lun, &id, crkey, scsi_type), Vec::new()),
        // Preempt and Preempt-and-Abort collapse — no task-manager
        // hook, identical visible transition (same as the SCSI side).
        // Preempting fences the prior holder / registrants → notify.
        wire::RACQA_PREEMPT | wire::RACQA_PREEMPT_ABORT => {
            let pre = mgr.snapshot(lun);
            let outcome = mgr.preempt(lun, &id, crkey, prkey, scsi_type);
            let events = reservation_events(mgr, ResvAction::Preempt, outcome, &pre, host_id, nsid);
            (outcome, events)
        }
        _ => return (fail(cid, StatusField::invalid_field()), Vec::new()),
    };
    (map_outcome(cid, outcome), events)
}

/// Reservation Release (0x15) — RRELA Release / Clear.
pub fn reservation_release(
    mgr: &ReservationManager,
    nsid: u32,
    host_id: [u8; 16],
    sqe: &Sqe,
    data_out: Option<&[u8]>,
) -> (NvmeResponse, Vec<ReservationEvent>) {
    let cid = sqe.cid;
    let lun = nsid_to_lun(nsid);
    let id = RegistrantId::nvme(host_id);

    let Some(crkey) = data_out.and_then(wire::parse_release_key) else {
        return (fail(cid, StatusField::invalid_field()), Vec::new());
    };
    let (outcome, events) = match wire::action(sqe.cdw10) {
        // Release drops the reservation but leaves registrations → the
        // other registrants get Reservation Released.
        wire::RRELA_RELEASE => {
            let Some(scsi_type) = wire::nvme_rtype_to_scsi_byte(wire::rtype(sqe.cdw10)) else {
                return (fail(cid, StatusField::invalid_field()), Vec::new());
            };
            let pre = mgr.snapshot(lun);
            let outcome = mgr.release(lun, &id, crkey, scsi_type);
            let events = reservation_events(mgr, ResvAction::Release, outcome, &pre, host_id, nsid);
            (outcome, events)
        }
        // Clear wipes every registration and the reservation → the
        // other registrants get Reservation Preempted.
        wire::RRELA_CLEAR => {
            let pre = mgr.snapshot(lun);
            let outcome = mgr.clear(lun, &id, crkey);
            let events = reservation_events(mgr, ResvAction::Clear, outcome, &pre, host_id, nsid);
            (outcome, events)
        }
        _ => return (fail(cid, StatusField::invalid_field()), Vec::new()),
    };
    (map_outcome(cid, outcome), events)
}

/// Reservation Report (0x0E) — builds the Reservation Status Data
/// Structure from a snapshot of the shared state. EDS (CDW11[0])
/// selects the extended 64-byte-per-controller form.
///
/// `cntlid_for_host` maps each registrant's HOSTID to a representative
/// CNTLID (its lowest live controller, or 0 if the host has a persisted
/// registration but no live controller — see #54). The registration is
/// host-keyed (one entry per HOSTID, not per controller), so the report
/// stays HOSTID-centric; only the CNTLID it reports is now faithful
/// rather than a static 1.
pub fn reservation_report(
    mgr: &ReservationManager,
    nsid: u32,
    sqe: &Sqe,
    data_in_max: u32,
    cntlid_for_host: impl Fn([u8; 16]) -> u16,
) -> NvmeResponse {
    let cid = sqe.cid;
    let lun = nsid_to_lun(nsid);
    let eds = wire::report_eds(sqe.cdw11);
    let snap = mgr.snapshot(lun);

    let rtype_nvme = snap
        .reservation_type
        .and_then(|t| wire::scsi_byte_to_nvme_rtype(t.as_u8()))
        .unwrap_or(0);
    let all_registrants = snap
        .reservation_type
        .is_some_and(ReservationType::is_all_registrants);

    let entries: Vec<wire::ReportEntry> = snap
        .registrants
        .iter()
        .map(|(id, key)| {
            let hostid = match id {
                RegistrantId::NvmeHost { hostid } => *hostid,
                RegistrantId::Iscsi { .. } => [0u8; 16],
            };
            let holds = all_registrants || snap.holder.as_ref() == Some(id);
            wire::ReportEntry {
                // A representative live CNTLID for this registrant's
                // host (0 if it has no live controller). The fencing
                // identity remains the HOSTID.
                cntlid: cntlid_for_host(hostid),
                holds_reservation: holds,
                hostid,
                rkey: *key,
            }
        })
        .collect();

    let mut payload =
        wire::reservation_status(snap.generation, rtype_nvme, &entries, eds, snap.aptpl);
    // NUMD (CDW10, 0-based dwords) sizes the host's buffer; clamp to
    // both it and the transport-supplied ceiling.
    let want = (sqe.cdw10 as usize).saturating_add(1).saturating_mul(4);
    let limit = want.min(data_in_max as usize);
    if payload.len() > limit {
        payload.truncate(limit);
    }
    NvmeResponse::with_data(Cqe::success(cid, 0, 0, 0), payload)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use scsi_spc::reservations::LunIdentity;

    use super::*;

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d =
            std::env::temp_dir().join(format!("thur-nvme-resv-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).expect("mkdir");
        d.join("reservations.json")
    }

    fn register_sqe(cptpl: u8) -> Sqe {
        let mut b = vec![0u8; nvme_base::SQE_SIZE];
        b[0] = 0x0D; // Reservation Register
        b[2] = 0x60; // CID
        b[4] = 0x01; // NSID = 1
        // CDW10: RREGA_REGISTER (0) | IEKEY (bit 3) | CPTPL (bits 30..31).
        let cdw10 = (1u32 << 3) | ((cptpl as u32) << 30);
        b[40..44].copy_from_slice(&cdw10.to_le_bytes());
        Sqe::parse(&b).unwrap()
    }

    fn keys(crkey: u64, nrkey: u64) -> Vec<u8> {
        let mut v = vec![0u8; 16];
        v[0..8].copy_from_slice(&crkey.to_le_bytes());
        v[8..16].copy_from_slice(&nrkey.to_le_bytes());
        v
    }

    #[test]
    fn cptpl_set_persists_and_survives_reload() {
        let path = tmp_path("cptpl-set");
        let host = [0xA1u8; 16];
        {
            let mgr = ReservationManager::load_from(path.clone(), Arc::new(LunIdentity));
            let sqe = register_sqe(wire::CPTPL_PERSIST);
            let (resp, _ev) = reservation_register(&mgr, 1, host, &sqe, Some(&keys(0, 0xCAFE)));
            assert!(
                resp.cqe.status == StatusField::SUCCESS,
                "register should succeed"
            );
            let snap = mgr.snapshot(0);
            assert!(snap.aptpl, "CPTPL=set must set the LU's PTPL state");
            assert_eq!(snap.registrants.len(), 1);
        }
        // Reload: the persisted registration (and PTPL state) survive.
        let mgr = ReservationManager::load_from(path, Arc::new(LunIdentity));
        let snap = mgr.snapshot(0);
        assert!(snap.aptpl);
        assert_eq!(snap.registrants, vec![(RegistrantId::nvme(host), 0xCAFE)]);
    }

    #[test]
    fn cptpl_set_rejected_when_not_capable() {
        // In-memory manager (no data dir): CPTPL=set is rejected, mirror
        // of the SCSI APTPL=1 reject and the RESCAP bit 0 = 0 advert.
        let mgr = ReservationManager::new();
        assert!(!mgr.ptpl_capable());
        let sqe = register_sqe(wire::CPTPL_PERSIST);
        let host = [0xB2u8; 16];
        let (resp, _ev) = reservation_register(&mgr, 1, host, &sqe, Some(&keys(0, 0xCAFE)));
        assert_eq!(resp.cqe.status, StatusField::invalid_field());
        assert!(mgr.snapshot(0).registrants.is_empty());
    }

    #[test]
    fn cptpl_clear_keeps_state_volatile() {
        // CPTPL=clear on a capable manager registers but does NOT persist.
        let path = tmp_path("cptpl-clear");
        let host = [0xC3u8; 16];
        {
            let mgr = ReservationManager::load_from(path.clone(), Arc::new(LunIdentity));
            let sqe = register_sqe(wire::CPTPL_CLEAR);
            let (resp, _ev) = reservation_register(&mgr, 1, host, &sqe, Some(&keys(0, 0xCAFE)));
            assert!(resp.cqe.status == StatusField::SUCCESS);
            assert!(!mgr.snapshot(0).aptpl);
        }
        // Nothing persisted => fresh load sees no registration.
        let mgr = ReservationManager::load_from(path, Arc::new(LunIdentity));
        assert!(mgr.snapshot(0).registrants.is_empty());
    }
}
