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
//! NSID maps onto the manager's `lun` key as `nsid as u64` — the
//! manager is private to the dispatcher, so only internal consistency
//! matters.

use nvme_base::reservation as wire;
use nvme_base::{Cqe, Sqe, StatusField};
use scsi_spc::pr::ReservationType;
use scsi_spc::reservations::{PrOutOutcome, RegistrantId, ReservationManager};

use crate::handler::NvmeResponse;

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
) -> NvmeResponse {
    let cid = sqe.cid;
    let lun = u64::from(nsid);
    let id = RegistrantId::nvme(host_id);

    // PTPL is not supported — reject a "set PTPL" request the same way
    // the SCSI side rejects APTPL=1.
    if wire::cptpl(sqe.cdw10) == wire::CPTPL_PERSIST {
        return fail(cid, StatusField::invalid_field());
    }
    let Some((crkey, nrkey)) = data_out.and_then(wire::parse_register_keys) else {
        return fail(cid, StatusField::invalid_field());
    };
    let iekey = wire::iekey(sqe.cdw10);
    let outcome = match wire::action(sqe.cdw10) {
        // Register and Replace both set the registration to NRKEY
        // after the CRKEY check (skipped when IEKEY=1).
        wire::RREGA_REGISTER | wire::RREGA_REPLACE => mgr.register(lun, &id, crkey, nrkey, iekey),
        // Unregister: NRKEY ignored, registration removed.
        wire::RREGA_UNREGISTER => mgr.register(lun, &id, crkey, 0, iekey),
        _ => return fail(cid, StatusField::invalid_field()),
    };
    map_outcome(cid, outcome)
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
) -> NvmeResponse {
    let cid = sqe.cid;
    let lun = u64::from(nsid);
    let id = RegistrantId::nvme(host_id);

    let Some(scsi_type) = wire::nvme_rtype_to_scsi_byte(wire::rtype(sqe.cdw10)) else {
        return fail(cid, StatusField::invalid_field());
    };
    let Some((crkey, prkey)) = data_out.and_then(wire::parse_acquire_keys) else {
        return fail(cid, StatusField::invalid_field());
    };
    let outcome = match wire::action(sqe.cdw10) {
        wire::RACQA_ACQUIRE => mgr.reserve(lun, &id, crkey, scsi_type),
        // Preempt and Preempt-and-Abort collapse — no task-manager
        // hook, identical visible transition (same as the SCSI side).
        wire::RACQA_PREEMPT | wire::RACQA_PREEMPT_ABORT => {
            mgr.preempt(lun, &id, crkey, prkey, scsi_type)
        }
        _ => return fail(cid, StatusField::invalid_field()),
    };
    map_outcome(cid, outcome)
}

/// Reservation Release (0x15) — RRELA Release / Clear.
pub fn reservation_release(
    mgr: &ReservationManager,
    nsid: u32,
    host_id: [u8; 16],
    sqe: &Sqe,
    data_out: Option<&[u8]>,
) -> NvmeResponse {
    let cid = sqe.cid;
    let lun = u64::from(nsid);
    let id = RegistrantId::nvme(host_id);

    let Some(crkey) = data_out.and_then(wire::parse_release_key) else {
        return fail(cid, StatusField::invalid_field());
    };
    let outcome = match wire::action(sqe.cdw10) {
        wire::RRELA_RELEASE => {
            let Some(scsi_type) = wire::nvme_rtype_to_scsi_byte(wire::rtype(sqe.cdw10)) else {
                return fail(cid, StatusField::invalid_field());
            };
            mgr.release(lun, &id, crkey, scsi_type)
        }
        wire::RRELA_CLEAR => mgr.clear(lun, &id, crkey),
        _ => return fail(cid, StatusField::invalid_field()),
    };
    map_outcome(cid, outcome)
}

/// Reservation Report (0x0E) — builds the Reservation Status Data
/// Structure from a snapshot of the shared state. EDS (CDW11[0])
/// selects the extended 64-byte-per-controller form.
pub fn reservation_report(
    mgr: &ReservationManager,
    nsid: u32,
    sqe: &Sqe,
    data_in_max: u32,
) -> NvmeResponse {
    let cid = sqe.cid;
    let lun = u64::from(nsid);
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
                // One CNTLID per connection today (static 1); the
                // registrant identity that fences is the HOSTID.
                cntlid: 1,
                holds_reservation: holds,
                hostid,
                rkey: *key,
            }
        })
        .collect();

    let mut payload = wire::reservation_status(snap.generation, rtype_nvme, &entries, eds);
    // NUMD (CDW10, 0-based dwords) sizes the host's buffer; clamp to
    // both it and the transport-supplied ceiling.
    let want = (sqe.cdw10 as usize).saturating_add(1).saturating_mul(4);
    let limit = want.min(data_in_max as usize);
    if payload.len() > limit {
        payload.truncate(limit);
    }
    NvmeResponse::with_data(Cqe::success(cid, 0, 0, 0), payload)
}
