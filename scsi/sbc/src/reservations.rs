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
    Nexus::new(req.tsih, req.initiator_iqn.map(str::to_owned))
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
        let Some(f) = reservations::parse_prout_cdb(req.cdb, req.data_out) else {
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
    }
}
