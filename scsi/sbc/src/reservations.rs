// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! SBC-3 PERSISTENT RESERVE IN / OUT (opcodes 0x5E / 0x5F).
//!
//! Multi-initiator block storage needs SCSI-3 persistent
//! reservations to coordinate failover (Windows Failover Cluster,
//! VMware vSphere SCSI-3 fencing, Pacemaker `fence_scsi`, ...).
//! thurvsa's tape sibling stubs PRIN out and outright rejects PROUT
//! because tape backup workflows are single-initiator; thurvsa's
//! block surface answers truthfully.
//!
//! ## State model
//!
//! Per LUN, the manager tracks:
//! - a set of **registrations** keyed by I_T nexus (TSIH +
//!   initiator IQN) — many-to-one between nexuses and reservation
//!   keys is supported (cooperating MPIO endpoints share a key);
//! - at most one **reservation** naming the holder nexus, the
//!   reservation key, and the SBC-3 type (0x01-0x08);
//! - a `PR_GENERATION` counter (SPC-4 §6.13.1.1) that increments
//!   on every successful PROUT.
//!
//! ## Persistence
//!
//! In-memory only. PTPL (persist through power loss) is advertised
//! as not capable in REPORT CAPABILITIES, so a daemon restart is
//! visible to initiators — they re-register on reconnect, which is
//! the well-trodden recovery path. Persisting reservation state to
//! disk is a separate ask and not on this branch.
//!
//! ## Scope coverage
//!
//! Only LU_SCOPE (0x00) is honored — element / extent scope are
//! historical and Windows / VMware / Linux cluster managers don't
//! exercise them. SBC-3 §5.10 specifies LU_SCOPE as the only
//! mandatory scope.

use std::collections::BTreeMap;
use std::sync::Mutex;

use core_block::PageCache;

use super::types::{ScsiRequest, ScsiResponse, SenseData};

/// I_T nexus identifier. Registrations are keyed by `(tsih,
/// initiator_iqn)` — TSIH alone is sufficient for uniqueness within
/// a running daemon (the iSCSI session manager allocates them
/// monotonically), but the IQN is preserved so READ FULL STATUS
/// can render an iSCSI-format TransportID and audit can name the
/// peer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Nexus {
    pub tsih: u16,
    pub initiator_iqn: Option<String>,
}

impl Nexus {
    /// Build a nexus identifier from an in-flight SCSI request.
    /// The dispatcher calls this once per command and threads the
    /// result through to the data-path enforcement helpers and the
    /// PROUT handlers.
    pub fn from_request(req: &ScsiRequest<'_>) -> Self {
        Self {
            tsih: req.tsih,
            initiator_iqn: req.initiator_iqn.map(str::to_owned),
        }
    }
}

/// SBC-3 reservation TYPE field (CDB byte 2 low nibble for PROUT,
/// header byte 13 low nibble for PRIN). Type 0x00 (no reservation)
/// is implicit via `Option<ReservationState>` on the LUN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ReservationType {
    WriteExclusive = 0x01,
    ExclusiveAccess = 0x03,
    WriteExclusiveRegistrantsOnly = 0x05,
    ExclusiveAccessRegistrantsOnly = 0x06,
    WriteExclusiveAllRegistrants = 0x07,
    ExclusiveAccessAllRegistrants = 0x08,
}

impl ReservationType {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::WriteExclusive),
            0x03 => Some(Self::ExclusiveAccess),
            0x05 => Some(Self::WriteExclusiveRegistrantsOnly),
            0x06 => Some(Self::ExclusiveAccessRegistrantsOnly),
            0x07 => Some(Self::WriteExclusiveAllRegistrants),
            0x08 => Some(Self::ExclusiveAccessAllRegistrants),
            _ => None,
        }
    }

    fn as_byte(self) -> u8 {
        self as u8
    }

    /// All-registrants types (WR_EX_AR / EX_AC_AR) survive holder
    /// disappearance as long as another registrant remains; for
    /// non-AR types the reservation is released when the holder
    /// unregisters.
    fn is_all_registrants(self) -> bool {
        matches!(
            self,
            Self::WriteExclusiveAllRegistrants | Self::ExclusiveAccessAllRegistrants
        )
    }
}

#[derive(Debug, Clone)]
struct ReservationState {
    holder: Nexus,
    key: u64,
    r#type: ReservationType,
}

#[derive(Default)]
struct LunState {
    /// `(tsih, initiator_iqn) -> reservation key`. Ordered map so
    /// READ KEYS / READ FULL STATUS render in a stable order across
    /// runs — initiators don't care, but it makes test diffs and
    /// audit logs readable.
    registrations: BTreeMap<(u16, Option<String>), u64>,
    reservation: Option<ReservationState>,
    /// SPC-4 §6.13.1.1 PR_GENERATION. Wraps on overflow per spec.
    generation: u32,
}

impl LunState {
    fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn registration_key(&self, nexus: &Nexus) -> Option<u64> {
        self.registrations
            .get(&(nexus.tsih, nexus.initiator_iqn.clone()))
            .copied()
    }

    fn is_registered(&self, nexus: &Nexus) -> bool {
        self.registration_key(nexus).is_some()
    }
}

/// Per-LUN registration / reservation state, mediated by a single
/// mutex. thurvsa's reservation traffic is rare relative to the data
/// path (one or two PROUTs per host boot + a quick PRIN sweep);
/// finer-grained locking would just be ceremony.
pub struct ReservationManager {
    by_lun: Mutex<BTreeMap<u64, LunState>>,
}

impl Default for ReservationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ReservationManager {
    pub fn new() -> Self {
        Self {
            by_lun: Mutex::new(BTreeMap::new()),
        }
    }

    /// Drop every registration owned by `tsih` across every LUN.
    /// Called from `ScsiHandler::on_session_close`. SPC-4 §5.13.4.2:
    /// when an I_T nexus disappears its registrations vanish; for
    /// non-AR reservations held by that nexus the reservation also
    /// releases. AR reservations persist as long as any other
    /// registrant remains — we just rotate the recorded holder
    /// stamp to one of the survivors.
    pub fn drop_nexus(&self, tsih: u16) {
        let mut map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
        for state in map.values_mut() {
            let removed: Vec<(u16, Option<String>)> = state
                .registrations
                .keys()
                .filter(|(t, _)| *t == tsih)
                .cloned()
                .collect();
            if removed.is_empty()
                && state
                    .reservation
                    .as_ref()
                    .is_none_or(|r| r.holder.tsih != tsih)
            {
                continue;
            }
            for k in &removed {
                state.registrations.remove(k);
            }
            if let Some(r) = state.reservation.clone()
                && r.holder.tsih == tsih
            {
                if r.r#type.is_all_registrants()
                    && let Some(((t, iqn), key)) = state.registrations.iter().next()
                {
                    state.reservation = Some(ReservationState {
                        holder: Nexus {
                            tsih: *t,
                            initiator_iqn: iqn.clone(),
                        },
                        key: *key,
                        r#type: r.r#type,
                    });
                } else {
                    state.reservation = None;
                }
            }
            state.bump_generation();
        }
    }

    /// Allow / deny a READ-side opcode (READ 10/16). The data path
    /// calls this before issuing the cloud fetch.
    pub fn allow_read(&self, lun: u64, nexus: &Nexus) -> bool {
        let map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
        let Some(state) = map.get(&lun) else {
            return true;
        };
        let Some(r) = &state.reservation else {
            return true;
        };
        match r.r#type {
            ReservationType::WriteExclusive
            | ReservationType::WriteExclusiveRegistrantsOnly
            | ReservationType::WriteExclusiveAllRegistrants => true,
            ReservationType::ExclusiveAccess => &r.holder == nexus,
            ReservationType::ExclusiveAccessRegistrantsOnly
            | ReservationType::ExclusiveAccessAllRegistrants => state.is_registered(nexus),
        }
    }

    /// Allow / deny a WRITE-side opcode (WRITE 10/16, SYNCHRONIZE
    /// CACHE 10/16). SBC-3 §5.10 — SYNCHRONIZE CACHE counts as a
    /// write-side check because it commits cached writes.
    pub fn allow_write(&self, lun: u64, nexus: &Nexus) -> bool {
        let map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
        let Some(state) = map.get(&lun) else {
            return true;
        };
        let Some(r) = &state.reservation else {
            return true;
        };
        match r.r#type {
            ReservationType::WriteExclusive | ReservationType::ExclusiveAccess => {
                &r.holder == nexus
            }
            ReservationType::WriteExclusiveRegistrantsOnly
            | ReservationType::ExclusiveAccessRegistrantsOnly
            | ReservationType::WriteExclusiveAllRegistrants
            | ReservationType::ExclusiveAccessAllRegistrants => state.is_registered(nexus),
        }
    }

    // ------------------------------------------------------------
    // PERSISTENT RESERVE IN (0x5E)
    // ------------------------------------------------------------

    /// Dispatch entry for opcode 0x5E. The dispatcher already
    /// resolved whether the LUN exists; we honour that with
    /// `lun_present` so READ KEYS against an unmapped LUN still
    /// returns LU NOT SUPPORTED (matches the rest of the surface).
    pub fn persistent_reserve_in(&self, req: &ScsiRequest<'_>, lun_present: bool) -> ScsiResponse {
        if !lun_present {
            return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
        }
        if req.cdb.len() < 10 {
            return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
        }
        let service_action = req.cdb[1] & 0x1F;
        let alloc = u16::from_be_bytes([req.cdb[7], req.cdb[8]]) as usize;
        let map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
        let state = map.get(&req.lun);
        let body = match service_action {
            0x00 => render_read_keys(state),
            0x01 => render_read_reservation(state),
            0x02 => render_report_capabilities(),
            0x03 => render_read_full_status(state),
            _ => return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB),
        };
        let truncated = truncate(body, alloc.min(req.data_in_max));
        ScsiResponse::good(truncated)
    }

    // ------------------------------------------------------------
    // PERSISTENT RESERVE OUT (0x5F)
    // ------------------------------------------------------------

    /// Dispatch entry for opcode 0x5F. Mutates per-LUN state.
    /// Returns RESERVATION CONFLICT for failed key checks (per
    /// SPC-4) and CHECK CONDITION + ILLEGAL REQUEST for malformed
    /// CDB / parameter list.
    pub fn persistent_reserve_out(
        &self,
        req: &ScsiRequest<'_>,
        cache: Option<&PageCache>,
        nexus: Nexus,
    ) -> ScsiResponse {
        if cache.is_none() {
            return ScsiResponse::check(SenseData::LU_NOT_SUPPORTED);
        }
        if req.cdb.len() < 10 {
            return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
        }
        let service_action = req.cdb[1] & 0x1F;
        let scope = (req.cdb[2] >> 4) & 0x0F;
        let type_byte = req.cdb[2] & 0x0F;
        let parameter_list_length =
            u32::from_be_bytes([req.cdb[5], req.cdb[6], req.cdb[7], req.cdb[8]]) as usize;

        // SBC-3: SCOPE must be LU_SCOPE (0x00) for the SAs we
        // support. Reject anything else as INVALID FIELD IN CDB.
        if scope != 0x00 && service_action != 0x00 && service_action != 0x06 {
            // REGISTER / REGISTER AND IGNORE EXISTING KEY ignore
            // SCOPE and TYPE per SPC-4 §6.14.1; everything else
            // requires LU_SCOPE.
            return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
        }

        // All supported PROUT service actions take a 24-byte
        // baseline parameter list. REGISTER AND MOVE (0x07) takes a
        // longer one; we don't support it.
        if parameter_list_length != 24 || req.data_out.len() < 24 {
            return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
        }
        let p = &req.data_out[..24];
        let reservation_key = u64::from_be_bytes(p[0..8].try_into().expect("8 bytes"));
        let service_action_key = u64::from_be_bytes(p[8..16].try_into().expect("8 bytes"));
        let aptpl = (p[20] & 0x01) != 0;
        let spec_i_pt = (p[20] & 0x08) != 0;
        let all_tg_pt = (p[20] & 0x04) != 0;

        // We don't support multi-port / SPEC_I_PT / APTPL on this
        // first cut — every initiator we care about (Windows
        // Failover Cluster, VMware, fence_scsi) sets APTPL=0 and
        // doesn't use SPEC_I_PT. Surface the truthful error so a
        // host that does request these features doesn't proceed
        // believing they were honored.
        if aptpl {
            return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
        }
        if spec_i_pt || all_tg_pt {
            return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
        }

        let mut map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
        let state = map.entry(req.lun).or_default();

        match service_action {
            0x00 => prout_register(state, &nexus, reservation_key, service_action_key, false),
            0x01 => prout_reserve(state, &nexus, reservation_key, type_byte),
            0x02 => prout_release(state, &nexus, reservation_key, type_byte),
            0x03 => prout_clear(state, &nexus, reservation_key),
            0x04 => prout_preempt(
                state,
                &nexus,
                reservation_key,
                service_action_key,
                type_byte,
            ),
            0x05 => prout_preempt(
                state,
                &nexus,
                reservation_key,
                service_action_key,
                type_byte,
            ),
            0x06 => prout_register(state, &nexus, reservation_key, service_action_key, true),
            0x07 => ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB),
            _ => ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB),
        }
    }
}

// ----------------------------------------------------------------
// PRIN renderers
// ----------------------------------------------------------------

/// 8-byte PRIN response header: PR_GENERATION (4) + ADDITIONAL
/// LENGTH (4). Used by READ KEYS, READ RESERVATION, and READ FULL
/// STATUS.
fn header(generation: u32, additional_length: u32) -> [u8; 8] {
    let mut h = [0u8; 8];
    h[0..4].copy_from_slice(&generation.to_be_bytes());
    h[4..8].copy_from_slice(&additional_length.to_be_bytes());
    h
}

fn render_read_keys(state: Option<&LunState>) -> Vec<u8> {
    let Some(state) = state else {
        return header(0, 0).to_vec();
    };
    let n = state.registrations.len() as u32;
    let mut out = header(state.generation, n * 8).to_vec();
    for key in state.registrations.values() {
        out.extend_from_slice(&key.to_be_bytes());
    }
    out
}

fn render_read_reservation(state: Option<&LunState>) -> Vec<u8> {
    let Some(state) = state else {
        return header(0, 0).to_vec();
    };
    let Some(r) = &state.reservation else {
        return header(state.generation, 0).to_vec();
    };
    let mut out = header(state.generation, 16).to_vec();
    out.extend_from_slice(&r.key.to_be_bytes());
    out.extend_from_slice(&[0u8; 4]); // obsolete (scope-specific addr)
    out.push(0); // reserved
    out.push(r.r#type.as_byte()); // SCOPE (LU_SCOPE = 0) | TYPE
    out.extend_from_slice(&[0u8; 2]); // obsolete
    out
}

fn render_report_capabilities() -> Vec<u8> {
    // SPC-4 Table 86 — REPORT CAPABILITIES parameter data.
    // 8 bytes total. We declare:
    //   PTPL_C    = 0  (no persist-through-power-loss)
    //   ATP_C     = 0  (ALL_TG_PT not supported)
    //   SIP_C     = 0  (SPEC_I_PT not supported)
    //   CRH       = 0  (no compatible-reservation handling for legacy
    //                   RESERVE(6) / RELEASE(6))
    //   TMV       = 1  (TYPE_MASK valid)
    //   ALLOW_CMDs = 0 (default — no extended allow-commands info)
    //   PTPL_A    = 0
    // TYPE_MASK exposes WR_EX, EX_AC, WR_EX_RO, EX_AC_RO, WR_EX_AR,
    // EX_AC_AR — every type thurvsa's ReservationType enum honors.
    let mut buf = vec![0u8; 8];
    buf[0] = 0x00;
    buf[1] = 0x08; // length = 8 bytes total minus the leading 0
    buf[2] = 0x00;
    buf[3] = 0x80; // TMV = 1
    buf[4] = 0xEA; // bit1 WR_EX, bit3 EX_AC, bit5 WR_EX_RO, bit6 EX_AC_RO, bit7 WR_EX_AR
    buf[5] = 0x01; // bit0 EX_AC_AR
    buf
}

fn render_read_full_status(state: Option<&LunState>) -> Vec<u8> {
    let Some(state) = state else {
        return header(0, 0).to_vec();
    };
    let mut descs = Vec::new();
    let holder_key = state.reservation.as_ref().map(|r| r.holder.clone());
    let res_type = state.reservation.as_ref().map(|r| r.r#type);
    for ((tsih, iqn), key) in &state.registrations {
        let nexus_here = Nexus {
            tsih: *tsih,
            initiator_iqn: iqn.clone(),
        };
        let is_holder = holder_key.as_ref() == Some(&nexus_here)
            || (res_type.is_some_and(|t| t.is_all_registrants()));
        descs.extend(full_status_descriptor(
            *key,
            is_holder,
            res_type,
            iqn.as_deref(),
        ));
    }
    let mut out = header(state.generation, descs.len() as u32).to_vec();
    out.extend_from_slice(&descs);
    out
}

/// One READ FULL STATUS descriptor: 24-byte fixed header + variable
/// TransportID. We render an iSCSI format-0 TransportID
/// (initiator IQN only; ISID is omitted) per SPC-4 §7.6.4.7.
fn full_status_descriptor(
    key: u64,
    is_holder: bool,
    res_type: Option<ReservationType>,
    iqn: Option<&str>,
) -> Vec<u8> {
    let transport_id = iscsi_transport_id(iqn);
    let mut out = Vec::with_capacity(24 + transport_id.len());
    out.extend_from_slice(&key.to_be_bytes());
    out.extend_from_slice(&[0u8; 4]); // reserved
    out.push(if is_holder { 0x01 } else { 0x00 }); // R_HOLDER bit, ALL_TG_PT=0
    out.push(if is_holder {
        res_type.map(|t| t.as_byte()).unwrap_or(0)
    } else {
        0
    });
    out.extend_from_slice(&[0u8; 4]); // reserved
    out.extend_from_slice(&[0u8; 2]); // RELATIVE TARGET PORT IDENTIFIER (single port)
    let tid_len = transport_id.len() as u32;
    out.extend_from_slice(&tid_len.to_be_bytes()); // ADDITIONAL DESCRIPTOR LENGTH
    out.extend_from_slice(&transport_id);
    out
}

/// SPC-4 §7.6.4.7 / RFC 3720 §3.2.6.1 iSCSI TransportID, format 0
/// (just the initiator IQN). The IQN is NUL-terminated and padded
/// to a 4-byte boundary, with the ADDITIONAL LENGTH field carrying
/// the padded length (excluding the 4-byte header).
fn iscsi_transport_id(iqn: Option<&str>) -> Vec<u8> {
    let iqn_bytes = iqn.unwrap_or("").as_bytes();
    let mut name = Vec::with_capacity(iqn_bytes.len() + 4);
    name.extend_from_slice(iqn_bytes);
    name.push(0); // NUL terminator
    while name.len() % 4 != 0 {
        name.push(0);
    }
    let mut tid = Vec::with_capacity(4 + name.len());
    tid.push(0x05); // PROTOCOL ID = iSCSI (5), FORMAT = 0
    tid.push(0x00); // reserved
    tid.extend_from_slice(&(name.len() as u16).to_be_bytes());
    tid.extend_from_slice(&name);
    tid
}

fn truncate(mut v: Vec<u8>, alloc: usize) -> Vec<u8> {
    if v.len() > alloc {
        v.truncate(alloc);
    }
    v
}

// ----------------------------------------------------------------
// PROUT service action handlers
// ----------------------------------------------------------------

/// REGISTER (0x00) and REGISTER AND IGNORE EXISTING KEY (0x06).
/// SBC-3 §6.14.2 — `ignore=false` requires the supplied
/// RESERVATION KEY to match the current registration (or zero if
/// not yet registered); `ignore=true` skips that check.
fn prout_register(
    state: &mut LunState,
    nexus: &Nexus,
    rk: u64,
    sark: u64,
    ignore: bool,
) -> ScsiResponse {
    if !ignore {
        let current = state.registration_key(nexus).unwrap_or(0);
        if current != rk {
            return ScsiResponse::reservation_conflict();
        }
    }
    let key = (nexus.tsih, nexus.initiator_iqn.clone());
    if sark == 0 {
        state.registrations.remove(&key);
        // Unregistration with the holder of a non-AR reservation
        // releases the reservation (SBC-3 §5.13.4.2).
        if let Some(r) = &state.reservation
            && &r.holder == nexus
        {
            if r.r#type.is_all_registrants() {
                if state.registrations.is_empty() {
                    state.reservation = None;
                } else if let Some(((t, iqn), key)) = state.registrations.iter().next() {
                    let new_holder = Nexus {
                        tsih: *t,
                        initiator_iqn: iqn.clone(),
                    };
                    let new_key = *key;
                    let r_type = r.r#type;
                    state.reservation = Some(ReservationState {
                        holder: new_holder,
                        key: new_key,
                        r#type: r_type,
                    });
                }
            } else {
                state.reservation = None;
            }
        }
    } else {
        state.registrations.insert(key, sark);
    }
    state.bump_generation();
    ScsiResponse::good(Vec::new())
}

/// RESERVE (0x01). Idempotent: re-RESERVE with the same nexus +
/// type + scope is a no-op success; conflicting RESERVE is
/// RESERVATION CONFLICT.
fn prout_reserve(state: &mut LunState, nexus: &Nexus, rk: u64, type_byte: u8) -> ScsiResponse {
    let Some(reg_key) = state.registration_key(nexus) else {
        return ScsiResponse::reservation_conflict();
    };
    if reg_key != rk {
        return ScsiResponse::reservation_conflict();
    }
    let Some(r#type) = ReservationType::from_byte(type_byte) else {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    };
    if let Some(existing) = &state.reservation {
        if existing.r#type == r#type && existing.holder == *nexus {
            return ScsiResponse::good(Vec::new()); // idempotent
        }
        return ScsiResponse::reservation_conflict();
    }
    state.reservation = Some(ReservationState {
        holder: nexus.clone(),
        key: rk,
        r#type,
    });
    state.bump_generation();
    ScsiResponse::good(Vec::new())
}

/// RELEASE (0x02). SBC-3 §6.14.4 — silent success when called by a
/// non-holder; conflict if the calling nexus isn't even
/// registered or supplied a stale key; conflict if the TYPE
/// supplied doesn't match the currently-held reservation's TYPE.
fn prout_release(state: &mut LunState, nexus: &Nexus, rk: u64, type_byte: u8) -> ScsiResponse {
    let Some(reg_key) = state.registration_key(nexus) else {
        return ScsiResponse::reservation_conflict();
    };
    if reg_key != rk {
        return ScsiResponse::reservation_conflict();
    }
    let Some(r#type) = ReservationType::from_byte(type_byte) else {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    };
    let Some(existing) = state.reservation.clone() else {
        return ScsiResponse::good(Vec::new());
    };
    if existing.r#type != r#type {
        return ScsiResponse::reservation_conflict();
    }
    let is_holder = existing.holder == *nexus
        || (existing.r#type.is_all_registrants() && state.is_registered(nexus));
    if !is_holder {
        return ScsiResponse::good(Vec::new()); // not the holder; no-op
    }
    state.reservation = None;
    state.bump_generation();
    ScsiResponse::good(Vec::new())
}

/// CLEAR (0x03). Wipes every registration and any reservation.
fn prout_clear(state: &mut LunState, nexus: &Nexus, rk: u64) -> ScsiResponse {
    let Some(reg_key) = state.registration_key(nexus) else {
        return ScsiResponse::reservation_conflict();
    };
    if reg_key != rk {
        return ScsiResponse::reservation_conflict();
    }
    state.registrations.clear();
    state.reservation = None;
    state.bump_generation();
    ScsiResponse::good(Vec::new())
}

/// PREEMPT (0x04) and PREEMPT AND ABORT (0x05). The "abort"
/// variant additionally aborts outstanding tasks for the preempted
/// nexus; thurvsa has no task manager hook today so the two
/// collapse to the same handler. The visible state transition is
/// identical.
fn prout_preempt(
    state: &mut LunState,
    nexus: &Nexus,
    rk: u64,
    sark: u64,
    type_byte: u8,
) -> ScsiResponse {
    let Some(reg_key) = state.registration_key(nexus) else {
        return ScsiResponse::reservation_conflict();
    };
    if reg_key != rk {
        return ScsiResponse::reservation_conflict();
    }
    let Some(r#type) = ReservationType::from_byte(type_byte) else {
        return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
    };
    // Drop every registration whose key matches SARK *except* the
    // calling nexus (the preemptor remains registered).
    let to_drop: Vec<(u16, Option<String>)> = state
        .registrations
        .iter()
        .filter(|((t, iqn), k)| {
            **k == sark && !(*t == nexus.tsih && iqn.as_deref() == nexus.initiator_iqn.as_deref())
        })
        .map(|(k, _)| k.clone())
        .collect();
    if to_drop.is_empty() && state.reservation.as_ref().is_none_or(|r| r.key != sark) {
        // SPC-4: PREEMPT with SARK that matches no registrant and
        // no reservation → RESERVATION CONFLICT.
        return ScsiResponse::reservation_conflict();
    }
    for k in &to_drop {
        state.registrations.remove(k);
    }
    // If the existing reservation was held by the preempted key,
    // install the calling nexus as the new holder under TYPE.
    let preempt_reservation = state.reservation.as_ref().is_some_and(|r| r.key == sark);
    if preempt_reservation || state.reservation.is_none() {
        state.reservation = Some(ReservationState {
            holder: nexus.clone(),
            key: rk,
            r#type,
        });
    }
    state.bump_generation();
    ScsiResponse::good(Vec::new())
}

// Test-only entry that skips the VolumeWriter guard. The full
// dispatch path runs through `persistent_reserve_out` which
// requires `Some(writer)`; unit tests below have no writer to hand
// in, so we expose a tiny wrapper that calls the same parameter-
// list parser and service-action arms.
#[cfg(test)]
impl ReservationManager {
    fn persistent_reserve_out_inner(&self, req: &ScsiRequest<'_>, nexus: Nexus) -> ScsiResponse {
        if req.cdb.len() < 10 {
            return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
        }
        let service_action = req.cdb[1] & 0x1F;
        let scope = (req.cdb[2] >> 4) & 0x0F;
        let type_byte = req.cdb[2] & 0x0F;
        let parameter_list_length =
            u32::from_be_bytes([req.cdb[5], req.cdb[6], req.cdb[7], req.cdb[8]]) as usize;
        if scope != 0x00 && service_action != 0x00 && service_action != 0x06 {
            return ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB);
        }
        if parameter_list_length != 24 || req.data_out.len() < 24 {
            return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
        }
        let p = &req.data_out[..24];
        let reservation_key = u64::from_be_bytes(p[0..8].try_into().expect("8 bytes"));
        let service_action_key = u64::from_be_bytes(p[8..16].try_into().expect("8 bytes"));
        let aptpl = (p[20] & 0x01) != 0;
        let spec_i_pt = (p[20] & 0x08) != 0;
        let all_tg_pt = (p[20] & 0x04) != 0;
        if aptpl || spec_i_pt || all_tg_pt {
            return ScsiResponse::check(SenseData::INVALID_FIELD_IN_PARAMETER_LIST);
        }
        let mut map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
        let state = map.entry(req.lun).or_default();
        match service_action {
            0x00 => prout_register(state, &nexus, reservation_key, service_action_key, false),
            0x01 => prout_reserve(state, &nexus, reservation_key, type_byte),
            0x02 => prout_release(state, &nexus, reservation_key, type_byte),
            0x03 => prout_clear(state, &nexus, reservation_key),
            0x04 | 0x05 => prout_preempt(
                state,
                &nexus,
                reservation_key,
                service_action_key,
                type_byte,
            ),
            0x06 => prout_register(state, &nexus, reservation_key, service_action_key, true),
            _ => ScsiResponse::check(SenseData::INVALID_FIELD_IN_CDB),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nexus(tsih: u16, iqn: &str) -> Nexus {
        Nexus {
            tsih,
            initiator_iqn: Some(iqn.to_string()),
        }
    }

    fn pr_in_cdb(sa: u8, alloc: u16) -> Vec<u8> {
        let mut cdb = vec![0u8; 10];
        cdb[0] = 0x5E;
        cdb[1] = sa & 0x1F;
        cdb[7..9].copy_from_slice(&alloc.to_be_bytes());
        cdb
    }

    fn pr_out_cdb(sa: u8, scope: u8, type_byte: u8) -> Vec<u8> {
        let mut cdb = vec![0u8; 10];
        cdb[0] = 0x5F;
        cdb[1] = sa & 0x1F;
        cdb[2] = ((scope & 0x0F) << 4) | (type_byte & 0x0F);
        cdb[5..9].copy_from_slice(&24u32.to_be_bytes());
        cdb
    }

    fn pr_out_params(rk: u64, sark: u64, aptpl: bool) -> Vec<u8> {
        let mut p = vec![0u8; 24];
        p[0..8].copy_from_slice(&rk.to_be_bytes());
        p[8..16].copy_from_slice(&sark.to_be_bytes());
        p[20] = if aptpl { 0x01 } else { 0x00 };
        p
    }

    fn req<'a>(cdb: &'a [u8], data_out: &'a [u8], tsih: u16, iqn: &'a str) -> ScsiRequest<'a> {
        ScsiRequest {
            lun: 0,
            cdb,
            data_out,
            data_in_max: 4096,
            tsih,
            initiator_iqn: Some(iqn),
            cid: 0,
            peer: "",
            session_partition: None,
        }
    }

    #[test]
    fn empty_state_read_keys_returns_zero_keys() {
        let mgr = ReservationManager::new();
        let cdb = pr_in_cdb(0x00, 64);
        let r = mgr.persistent_reserve_in(&req(&cdb, &[], 1, "iqn.test:a"), true);
        assert_eq!(r.data_in.len(), 8);
        assert_eq!(&r.data_in[4..8], &0u32.to_be_bytes());
    }

    #[test]
    fn report_capabilities_advertises_six_types() {
        let mgr = ReservationManager::new();
        let cdb = pr_in_cdb(0x02, 64);
        let r = mgr.persistent_reserve_in(&req(&cdb, &[], 1, "iqn.test:a"), true);
        assert_eq!(r.data_in.len(), 8);
        assert_eq!(r.data_in[1], 0x08);
        assert_eq!(r.data_in[3], 0x80); // TMV
        assert_eq!(r.data_in[4], 0xEA);
        assert_eq!(r.data_in[5], 0x01);
    }

    #[test]
    fn register_then_read_keys_lists_the_key() {
        let mgr = ReservationManager::new();
        let n = nexus(1, "iqn.test:a");
        let prout = pr_out_cdb(0x00, 0, 0);
        let params = pr_out_params(0, 0xDEADBEEF, false);
        // VolumeWriter present sentinel: we don't have a real one
        // in the unit test, so call the inner helpers directly to
        // bypass the writer guard.
        let r = mgr.persistent_reserve_out_inner(&req(&prout, &params, 1, "iqn.test:a"), n);
        assert_eq!(r.status, scsi_spc::scsi::ScsiStatus::Good);
        let cdb = pr_in_cdb(0x00, 64);
        let r = mgr.persistent_reserve_in(&req(&cdb, &[], 1, "iqn.test:a"), true);
        // header(8) + 1 key(8) = 16 bytes
        assert_eq!(r.data_in.len(), 16);
        assert_eq!(&r.data_in[4..8], &8u32.to_be_bytes());
        assert_eq!(&r.data_in[8..16], &0xDEADBEEFu64.to_be_bytes());
    }

    #[test]
    fn reserve_blocks_unregistered_writer() {
        let mgr = ReservationManager::new();
        let na = nexus(1, "iqn.test:a");
        let nb = nexus(2, "iqn.test:b");
        // A registers + reserves WRITE_EXCLUSIVE.
        register(&mgr, &na, 0xAAAA);
        reserve(&mgr, &na, 0xAAAA, ReservationType::WriteExclusive.as_byte());
        // B is not registered → write must be denied; A may write.
        assert!(!mgr.allow_write(0, &nb));
        assert!(mgr.allow_write(0, &na));
        // Reads are allowed under WRITE_EXCLUSIVE for everyone.
        assert!(mgr.allow_read(0, &nb));
        assert!(mgr.allow_read(0, &na));
    }

    #[test]
    fn exclusive_access_blocks_both_reads_and_writes() {
        let mgr = ReservationManager::new();
        let na = nexus(1, "iqn.test:a");
        let nb = nexus(2, "iqn.test:b");
        register(&mgr, &na, 0xAAAA);
        reserve(
            &mgr,
            &na,
            0xAAAA,
            ReservationType::ExclusiveAccess.as_byte(),
        );
        assert!(!mgr.allow_read(0, &nb));
        assert!(!mgr.allow_write(0, &nb));
        assert!(mgr.allow_read(0, &na));
        assert!(mgr.allow_write(0, &na));
    }

    #[test]
    fn registrants_only_lets_other_registrants_through() {
        let mgr = ReservationManager::new();
        let na = nexus(1, "iqn.test:a");
        let nb = nexus(2, "iqn.test:b");
        let nc = nexus(3, "iqn.test:c");
        register(&mgr, &na, 0xAAAA);
        register(&mgr, &nb, 0xBBBB);
        reserve(
            &mgr,
            &na,
            0xAAAA,
            ReservationType::WriteExclusiveRegistrantsOnly.as_byte(),
        );
        // B is registered → may write; C is not → blocked.
        assert!(mgr.allow_write(0, &nb));
        assert!(!mgr.allow_write(0, &nc));
        // Reads always allowed under WR_EX_RO.
        assert!(mgr.allow_read(0, &nc));
    }

    #[test]
    fn release_clears_reservation() {
        let mgr = ReservationManager::new();
        let na = nexus(1, "iqn.test:a");
        register(&mgr, &na, 0xAAAA);
        reserve(
            &mgr,
            &na,
            0xAAAA,
            ReservationType::ExclusiveAccess.as_byte(),
        );
        let cdb = pr_out_cdb(0x02, 0, ReservationType::ExclusiveAccess.as_byte());
        let params = pr_out_params(0xAAAA, 0, false);
        let r = mgr.persistent_reserve_out_inner(&req(&cdb, &params, 1, "iqn.test:a"), na.clone());
        assert_eq!(r.status, scsi_spc::scsi::ScsiStatus::Good);
        assert!(mgr.allow_write(0, &nexus(2, "iqn.test:b")));
    }

    #[test]
    fn drop_nexus_removes_all_registrations_for_tsih() {
        let mgr = ReservationManager::new();
        let na = nexus(1, "iqn.test:a");
        let nb = nexus(2, "iqn.test:b");
        register(&mgr, &na, 0xAAAA);
        register(&mgr, &nb, 0xBBBB);
        reserve(
            &mgr,
            &na,
            0xAAAA,
            ReservationType::ExclusiveAccess.as_byte(),
        );
        mgr.drop_nexus(1);
        // A's key is gone; B remains.
        let cdb = pr_in_cdb(0x00, 64);
        let r = mgr.persistent_reserve_in(&req(&cdb, &[], 2, "iqn.test:b"), true);
        assert_eq!(r.data_in.len(), 16);
        assert_eq!(&r.data_in[8..16], &0xBBBBu64.to_be_bytes());
        // Reservation released because A held a non-AR type.
        assert!(mgr.allow_write(0, &nb));
    }

    #[test]
    fn preempt_replaces_holder() {
        let mgr = ReservationManager::new();
        let na = nexus(1, "iqn.test:a");
        let nb = nexus(2, "iqn.test:b");
        register(&mgr, &na, 0xAAAA);
        register(&mgr, &nb, 0xBBBB);
        reserve(
            &mgr,
            &na,
            0xAAAA,
            ReservationType::ExclusiveAccess.as_byte(),
        );
        // B preempts A.
        let cdb = pr_out_cdb(0x04, 0, ReservationType::WriteExclusive.as_byte());
        let params = pr_out_params(0xBBBB, 0xAAAA, false);
        let r = mgr.persistent_reserve_out_inner(&req(&cdb, &params, 2, "iqn.test:b"), nb.clone());
        assert_eq!(r.status, scsi_spc::scsi::ScsiStatus::Good);
        // A is no longer registered; A's writes are now blocked.
        assert!(!mgr.allow_write(0, &na));
        assert!(mgr.allow_write(0, &nb));
    }

    // Test helpers that bypass the writer guard.
    fn register(mgr: &ReservationManager, n: &Nexus, key: u64) {
        let cdb = pr_out_cdb(0x06, 0, 0); // REGISTER AND IGNORE EXISTING KEY
        let params = pr_out_params(0, key, false);
        let req = ScsiRequest {
            lun: 0,
            cdb: &cdb,
            data_out: &params,
            data_in_max: 0,
            tsih: n.tsih,
            initiator_iqn: n.initiator_iqn.as_deref(),
            cid: 0,
            peer: "",
            session_partition: None,
        };
        let r = mgr.persistent_reserve_out_inner(&req, n.clone());
        assert_eq!(r.status, scsi_spc::scsi::ScsiStatus::Good);
    }
    fn reserve(mgr: &ReservationManager, n: &Nexus, key: u64, type_byte: u8) {
        let cdb = pr_out_cdb(0x01, 0, type_byte);
        let params = pr_out_params(key, 0, false);
        let req = ScsiRequest {
            lun: 0,
            cdb: &cdb,
            data_out: &params,
            data_in_max: 0,
            tsih: n.tsih,
            initiator_iqn: n.initiator_iqn.as_deref(),
            cid: 0,
            peer: "",
            session_partition: None,
        };
        let r = mgr.persistent_reserve_out_inner(&req, n.clone());
        assert_eq!(r.status, scsi_spc::scsi::ScsiStatus::Good);
    }
}
