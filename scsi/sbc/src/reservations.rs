// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! SBC adapter over the shared PERSISTENT RESERVE state machine.
//!
//! The registration / reservation bookkeeping, the PROUT service-
//! action handlers, the PRIN renderers, and the `allow_read` /
//! `allow_write` enforcement gates all live in
//! [`scsi_spc::reservations`] so the block (thurvsa) and tape
//! (thurvtl) surfaces share one implementation. This module is the
//! thin block-side adapter: it builds a [`Nexus`] from the SBC
//! [`ScsiRequest`], slices the 0x5E / 0x5F CDBs via the shared
//! parsers, and maps the neutral [`PrInOutcome`] / [`PrOutOutcome`]
//! results onto `ScsiResponse` / `SenseData`.
//!
//! `Nexus`, `ReservationManager`, `allow_read`, `allow_write`, and
//! `drop_nexus` are re-exported / resolve directly from scsi-spc, so
//! `dispatcher.rs` and `data_path.rs` reach them unchanged.

use core_block::PageCache;
use scsi_spc::reservations::{self, PrInOutcome, PrOutOutcome};

use super::types::{ScsiRequest, ScsiResponse, SenseData};

pub use scsi_spc::reservations::{Nexus, ReservationManager};

/// Build a nexus identifier from an in-flight SBC SCSI request. The
/// dispatcher calls this once per command and threads the result
/// through to the data-path enforcement helpers and the PROUT
/// handler. (A free fn rather than `Nexus::from_request` because
/// `Nexus` is now foreign to this crate.)
pub fn nexus_from_request(req: &ScsiRequest<'_>) -> Nexus {
    Nexus::iscsi(req.initiator_iqn.map(str::to_owned), req.initiator_isid)
}

/// SBC-flavored wrappers over the neutral PR core. Exposed as a
/// trait so the dispatcher's existing
/// `self.reservations.persistent_reserve_{in,out}(...)` call sites
/// resolve unchanged (the inherent neutral methods are named
/// `prin` / `prout` precisely so these names don't collide).
pub trait SbcReservations {
    fn persistent_reserve_in(&self, req: &ScsiRequest<'_>, lun_present: bool) -> ScsiResponse;
    fn persistent_reserve_out(
        &self,
        req: &ScsiRequest<'_>,
        cache: Option<&PageCache>,
        nexus: Nexus,
    ) -> ScsiResponse;
}

impl SbcReservations for ReservationManager {
    fn persistent_reserve_in(&self, req: &ScsiRequest<'_>, lun_present: bool) -> ScsiResponse {
        // LUN-present check first (matches the pre-hoist ordering):
        // an unmapped / unadmitted LUN answers LU NOT SUPPORTED before
        // we look at the CDB.
        if !lun_present {
            return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
        }
        let Some((service_action, alloc)) = reservations::parse_prin_cdb(req.cdb) else {
            return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
        };
        match self.prin(req.lun, service_action, true) {
            PrInOutcome::Good(mut body) => {
                let limit = alloc.min(req.data_in_max);
                if body.len() > limit {
                    body.truncate(limit);
                }
                ScsiResponse::good(body)
            }
            PrInOutcome::InvalidFieldInCdb => ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB),
            PrInOutcome::LuNotSupported => ScsiResponse::check(SenseData::LU_NOT_SUPPORTED),
        }
    }

    fn persistent_reserve_out(
        &self,
        req: &ScsiRequest<'_>,
        cache: Option<&PageCache>,
        nexus: Nexus,
    ) -> ScsiResponse {
        if cache.is_none() {
            return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
        }
        let Some(f) = reservations::parse_prout_cdb(req.cdb, &req.data_out) else {
            return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
        };
        let outcome = self.prout(
            req.lun,
            f.service_action,
            f.scope,
            f.type_byte,
            f.param_list,
            f.param_list_len,
            &nexus,
            true,
        );
        map_prout(outcome)
    }
}

/// Map the neutral PROUT outcome onto the SBC `ScsiResponse`.
fn map_prout(outcome: PrOutOutcome) -> ScsiResponse {
    match outcome {
        PrOutOutcome::Good => ScsiResponse::good(Vec::new()),
        PrOutOutcome::ReservationConflict => ScsiResponse::reservation_conflict(),
        PrOutOutcome::InvalidFieldInCdb => ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB),
        PrOutOutcome::InvalidFieldInParameterList => {
            ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST)
        }
        PrOutOutcome::LuNotSupported => ScsiResponse::check(SenseData::LU_NOT_SUPPORTED),
        // PTPL persist-before-ack failed: do not ack GOOD — the host
        // must know the fence was not made durable.
        PrOutOutcome::PersistFailed => ScsiResponse::check(SenseData::INTERNAL_TARGET_FAILURE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scsi_spc::scsi::ScsiStatus;

    /// Build a minimal SBC `ScsiRequest` for the PR adapter — only the
    /// fields the PRIN / PROUT paths read (`lun`, `cdb`, `data_out`,
    /// `data_in_max`, `initiator_iqn`, `initiator_isid`) carry meaning;
    /// the rest are inert.
    fn req<'a>(cdb: &'a [u8], data_out: &[u8], data_in_max: usize) -> ScsiRequest<'a> {
        ScsiRequest {
            tsih: 1,
            cid: 0,
            lun: 0,
            cdb,
            data_out: data_out.to_vec(),
            data_in_max,
            initiator_iqn: Some("iqn.2025-10.com.metebalci:host-a"),
            initiator_isid: [1, 2, 3, 4, 5, 6],
            peer: "10.0.0.1:3260",
            session_partition: None,
            session_volumes: None,
        }
    }

    // The single most safety-relevant assertion in this adapter: every
    // neutral PROUT outcome maps to exactly one SBC response, and
    // PersistFailed (PTPL persist-before-ack failure, issue #57) must
    // surface CHECK CONDITION / INTERNAL TARGET FAILURE — NOT GOOD. A
    // wrong arm here silently tells a cluster member its reservation
    // fence is durable when it is not.
    #[test]
    fn map_prout_maps_every_outcome() {
        assert_eq!(
            map_prout(PrOutOutcome::Good),
            ScsiResponse::good(Vec::new())
        );
        assert_eq!(
            map_prout(PrOutOutcome::ReservationConflict),
            ScsiResponse::reservation_conflict()
        );
        assert_eq!(
            map_prout(PrOutOutcome::InvalidFieldInCdb),
            ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB)
        );
        assert_eq!(
            map_prout(PrOutOutcome::InvalidFieldInParameterList),
            ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST)
        );
        assert_eq!(
            map_prout(PrOutOutcome::LuNotSupported),
            ScsiResponse::check(SenseData::LU_NOT_SUPPORTED)
        );

        let persist_failed = map_prout(PrOutOutcome::PersistFailed);
        assert_eq!(
            persist_failed,
            ScsiResponse::check(SenseData::INTERNAL_TARGET_FAILURE)
        );
        assert_ne!(
            persist_failed.status,
            ScsiStatus::Good,
            "a durability failure must never be acked GOOD"
        );
    }

    #[test]
    fn nexus_from_request_carries_iscsi_identity() {
        let cdb = [0u8; 10];
        let r = req(&cdb, &[], 0);
        let nexus = nexus_from_request(&r);
        // Same IQN+ISID must reproduce the same nexus (the stable
        // initiator-port identity reservations are keyed on); a
        // different ISID must not.
        assert_eq!(nexus, nexus_from_request(&r));
        let mut other = req(&cdb, &[], 0);
        other.initiator_isid = [9, 9, 9, 9, 9, 9];
        assert_ne!(nexus, nexus_from_request(&other));
    }

    #[test]
    fn prin_on_absent_lun_is_lu_not_supported() {
        let mgr = ReservationManager::new();
        // A well-formed READ KEYS CDB, but the LUN isn't mapped — the
        // adapter answers LU NOT SUPPORTED before parsing the CDB.
        let cdb = [0x5E, 0x00, 0, 0, 0, 0, 0, 0x00, 0x08, 0];
        let resp = mgr.persistent_reserve_in(&req(&cdb, &[], 4096), false);
        assert_eq!(resp, ScsiResponse::check(SenseData::LU_NOT_SUPPORTED));
    }

    #[test]
    fn prin_with_unparseable_cdb_is_invalid_field() {
        let mgr = ReservationManager::new();
        // CDB shorter than 10 bytes -> parse_prin_cdb returns None.
        let cdb = [0x5E, 0x00, 0, 0];
        let resp = mgr.persistent_reserve_in(&req(&cdb, &[], 4096), true);
        assert_eq!(resp, ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB));
    }

    #[test]
    fn prin_read_keys_truncates_body_to_alloc_min_datainmax() {
        let mgr = ReservationManager::new();
        // READ KEYS on an empty manager yields the 8-byte header
        // (PRgeneration + ADDITIONAL LENGTH, zero keys). Allocation
        // length = 4 in the CDB caps the returned body to 4 bytes even
        // though data_in_max is large.
        let cdb = [0x5E, 0x00, 0, 0, 0, 0, 0, 0x00, 0x04, 0];
        let resp = mgr.persistent_reserve_in(&req(&cdb, &[], 4096), true);
        assert_eq!(resp.status, ScsiStatus::Good);
        assert_eq!(resp.data_in.len(), 4, "alloc=4 must cap the body");
    }

    #[test]
    fn prout_without_cache_is_lu_not_supported() {
        let mgr = ReservationManager::new();
        // PROUT against an unmapped LUN (cache None) — refused before
        // the CDB is parsed.
        let cdb = [0x5F, 0x00, 0, 0, 0, 0, 0, 0, 0x18, 0];
        let nexus = Nexus::iscsi(Some("iqn.x:h".to_owned()), [0; 6]);
        let resp = mgr.persistent_reserve_out(&req(&cdb, &[], 0), None, nexus);
        assert_eq!(resp, ScsiResponse::check(SenseData::LU_NOT_SUPPORTED));
    }
}
