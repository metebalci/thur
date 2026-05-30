// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Transport-neutral SCSI PERSISTENT RESERVE state machine
//! (SPC-4 §6.16, opcodes 0x5E / 0x5F).
//!
//! Multi-initiator storage needs SCSI-3 persistent reservations to
//! coordinate failover (Windows Failover Cluster, VMware vSphere
//! SCSI-3 fencing, Pacemaker `fence_scsi`, clustered backup
//! directors). Both products surface PR over the same I_T-nexus
//! model, so the bookkeeping lives here once and each product's
//! dispatcher is a thin adapter that parses its own CDB / Data-Out
//! and maps the [`PrInOutcome`] / [`PrOutOutcome`] results onto its
//! own response type. thurvsa (block) and thurvtl (tape drive LUN)
//! both consume this — keeping the state machine here means the two
//! surfaces can't drift.
//!
//! ## State model
//!
//! Per LUN, the manager tracks:
//! - a set of **registrations** keyed by I_T nexus (TSIH +
//!   initiator IQN) — many-to-one between nexuses and reservation
//!   keys is supported (cooperating MPIO endpoints share a key);
//! - at most one **reservation** naming the holder nexus, the
//!   reservation key, and the type (0x01-0x08);
//! - a `PR_GENERATION` counter (SPC-4 §6.13.1.1) that increments
//!   on every successful PROUT.
//!
//! ## Persistence
//!
//! In-memory only. PTPL (persist through power loss) is advertised
//! as not capable in REPORT CAPABILITIES, so a daemon restart is
//! visible to initiators — they re-register on reconnect, which is
//! the well-trodden recovery path.
//!
//! ## Scope coverage
//!
//! Only LU_SCOPE (0x00) is honored — element / extent scope are
//! historical and Windows / VMware / Linux cluster managers don't
//! exercise them.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::pr::ReservationType;

/// Registrant identity. A registration (and any reservation it
/// holds) is named by the transport-specific identity of the host
/// that created it:
///
/// - **iSCSI** I_T nexus: `(tsih, iqn)`. TSIH alone is sufficient for
///   uniqueness within a running daemon (the session manager
///   allocates them monotonically), but the IQN is preserved so READ
///   FULL STATUS can render an iSCSI-format TransportID and audit can
///   name the peer. Dropped on session close (see [`ReservationManager::drop_nexus`]).
/// - **NVMe** host: the 128-bit Host Identifier from the Fabrics
///   Connect data. One NVMe host opens many controller/queue
///   associations (each its own TCP connection) under a single
///   HOSTID, so the registrant is the *host*, not the connection —
///   and unlike the iSCSI nexus it does **not** drop on connection
///   teardown. A registration persists until explicit Release /
///   Register-unregister / Preempt (or daemon restart, since PTPL is
///   not advertised).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RegistrantId {
    Iscsi { tsih: u16, iqn: Option<String> },
    NvmeHost { hostid: [u8; 16] },
}

/// Back-compatible name for the SCSI dispatchers (iSCSI block, tape
/// drive / changer LUN), which only ever construct the iSCSI variant.
pub type Nexus = RegistrantId;

impl RegistrantId {
    /// Build an iSCSI I_T nexus from `tsih` + the login-advertised
    /// IQN. (Named `new` so the existing `Nexus::new` call sites in
    /// the SCSI dispatchers keep working unchanged.)
    pub fn new(tsih: u16, initiator_iqn: Option<String>) -> Self {
        Self::Iscsi {
            tsih,
            iqn: initiator_iqn,
        }
    }

    /// Build an NVMe host registrant from the 128-bit Connect HOSTID.
    pub fn nvme(hostid: [u8; 16]) -> Self {
        Self::NvmeHost { hostid }
    }
}

/// Outcome of a PERSISTENT RESERVE OUT. Response-type-neutral — the
/// per-product adapter maps each variant onto its own response
/// (`ScsiResponse::reservation_conflict()`, `ScsiResp::check_condition()`,
/// …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrOutOutcome {
    /// GOOD — state mutated (or idempotent no-op).
    Good,
    /// RESERVATION CONFLICT (SAM-5 0x18) — key check failed.
    ReservationConflict,
    /// CHECK CONDITION / ILLEGAL REQUEST — INVALID FIELD IN CDB.
    InvalidFieldInCdb,
    /// CHECK CONDITION / ILLEGAL REQUEST — INVALID FIELD IN PARAMETER LIST.
    InvalidFieldInParameterList,
    /// CHECK CONDITION / ILLEGAL REQUEST — LOGICAL UNIT NOT SUPPORTED.
    LuNotSupported,
}

/// Outcome of a PERSISTENT RESERVE IN. On success carries the
/// already-rendered response body; the adapter truncates it to its
/// own allocation length before replying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrInOutcome {
    /// GOOD with the rendered PRIN body (READ KEYS / READ RESERVATION
    /// / REPORT CAPABILITIES / READ FULL STATUS).
    Good(Vec<u8>),
    /// CHECK CONDITION / ILLEGAL REQUEST — INVALID FIELD IN CDB
    /// (unknown service action).
    InvalidFieldInCdb,
    /// CHECK CONDITION / ILLEGAL REQUEST — LOGICAL UNIT NOT SUPPORTED.
    LuNotSupported,
}

/// Read-only snapshot of a LUN's reservation state. Lets a renderer
/// build a transport-specific wire structure (e.g. the NVMe
/// Reservation Report) from the shared bookkeeping without
/// duplicating it. The SCSI PRIN renderers read [`LunState`] directly
/// and don't use this.
#[derive(Debug, Clone)]
pub struct ReservationSnapshot {
    pub generation: u32,
    pub reservation_type: Option<ReservationType>,
    pub holder: Option<RegistrantId>,
    /// `(registrant, reservation key)` in stable BTreeMap order.
    pub registrants: Vec<(RegistrantId, u64)>,
}

/// Parsed PERSISTENT RESERVE OUT CDB + Data-Out fields. Both
/// products use identical byte offsets, so [`parse_prout_cdb`] is the
/// single source of truth for the slicing.
pub struct PrOutFields<'a> {
    pub service_action: u8,
    pub scope: u8,
    pub type_byte: u8,
    pub param_list: &'a [u8],
    pub param_list_len: u32,
}

/// Slice a 0x5F CDB + Data-Out into the neutral PROUT inputs.
/// Returns `None` when the CDB is shorter than the 10-byte PROUT
/// minimum — the adapter maps that to INVALID FIELD IN CDB.
pub fn parse_prout_cdb<'a>(cdb: &[u8], data_out: &'a [u8]) -> Option<PrOutFields<'a>> {
    if cdb.len() < 10 {
        return None;
    }
    Some(PrOutFields {
        service_action: cdb[1] & 0x1F,
        scope: (cdb[2] >> 4) & 0x0F,
        type_byte: cdb[2] & 0x0F,
        param_list: data_out,
        param_list_len: u32::from_be_bytes([cdb[5], cdb[6], cdb[7], cdb[8]]),
    })
}

/// Slice a 0x5E CDB into `(service_action, allocation_length)`.
/// Returns `None` when the CDB is shorter than the 10-byte PRIN
/// minimum.
pub fn parse_prin_cdb(cdb: &[u8]) -> Option<(u8, usize)> {
    if cdb.len() < 10 {
        return None;
    }
    let service_action = cdb[1] & 0x1F;
    let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
    Some((service_action, alloc))
}

#[derive(Debug, Clone)]
struct ReservationState {
    holder: Nexus,
    key: u64,
    r#type: ReservationType,
}

#[derive(Default)]
struct LunState {
    /// `registrant -> reservation key`. Ordered map so READ KEYS /
    /// READ FULL STATUS render in a stable order across runs —
    /// initiators don't care, but it makes test diffs and audit logs
    /// readable.
    registrations: BTreeMap<RegistrantId, u64>,
    reservation: Option<ReservationState>,
    /// SPC-4 §6.13.1.1 PR_GENERATION. Wraps on overflow per spec.
    generation: u32,
}

impl LunState {
    fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn registration_key(&self, id: &RegistrantId) -> Option<u64> {
        self.registrations.get(id).copied()
    }

    fn is_registered(&self, id: &RegistrantId) -> bool {
        self.registration_key(id).is_some()
    }
}

/// Per-LUN registration / reservation state, mediated by a single
/// mutex. Reservation traffic is rare relative to the data path (one
/// or two PROUTs per host boot + a quick PRIN sweep); finer-grained
/// locking would just be ceremony.
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
            let removed: Vec<RegistrantId> = state
                .registrations
                .keys()
                .filter(|id| matches!(id, RegistrantId::Iscsi { tsih: t, .. } if *t == tsih))
                .cloned()
                .collect();
            let holder_is_tsih = state.reservation.as_ref().is_some_and(
                |r| matches!(&r.holder, RegistrantId::Iscsi { tsih: t, .. } if *t == tsih),
            );
            if removed.is_empty() && !holder_is_tsih {
                continue;
            }
            for k in &removed {
                state.registrations.remove(k);
            }
            if let Some(r) = state.reservation.clone()
                && matches!(&r.holder, RegistrantId::Iscsi { tsih: t, .. } if *t == tsih)
            {
                if r.r#type.is_all_registrants()
                    && let Some((id, key)) = state.registrations.iter().next()
                {
                    state.reservation = Some(ReservationState {
                        holder: id.clone(),
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

    /// Allow / deny a READ-side opcode. The data path calls this
    /// before issuing the fetch / medium read.
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

    /// Allow / deny a WRITE-side opcode. SBC-3 §5.10 — SYNCHRONIZE
    /// CACHE counts as a write-side check because it commits cached
    /// writes; on tape, medium-mutating opcodes (WRITE, WRITE
    /// FILEMARKS, ERASE, FORMAT MEDIUM) gate the same way.
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

    /// Neutral PRIN. `service_action` is `cdb[1] & 0x1F` (already
    /// extracted by [`parse_prin_cdb`]); the returned body is
    /// un-truncated so the adapter can apply its own allocation /
    /// EDTL clamp. Named `prin` (not `persistent_reserve_in`) so a
    /// product adapter can expose a same-named wrapper method without
    /// shadowing this inherent one.
    pub fn prin(&self, lun: u64, service_action: u8, lun_present: bool) -> PrInOutcome {
        if !lun_present {
            return PrInOutcome::LuNotSupported;
        }
        let map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
        let state = map.get(&lun);
        let body = match service_action {
            0x00 => render_read_keys(state),
            0x01 => render_read_reservation(state),
            0x02 => render_report_capabilities(),
            0x03 => render_read_full_status(state),
            _ => return PrInOutcome::InvalidFieldInCdb,
        };
        PrInOutcome::Good(body)
    }

    // ------------------------------------------------------------
    // PERSISTENT RESERVE OUT (0x5F)
    // ------------------------------------------------------------

    /// Neutral PROUT. Takes the already-extracted CDB fields (see
    /// [`parse_prout_cdb`]) plus the calling nexus, mutates per-LUN
    /// state, and returns the response-neutral [`PrOutOutcome`].
    /// Named `prout` (not `persistent_reserve_out`) so a product
    /// adapter can expose a same-named wrapper method without
    /// shadowing this inherent one.
    #[allow(clippy::too_many_arguments)]
    pub fn prout(
        &self,
        lun: u64,
        service_action: u8,
        scope: u8,
        type_byte: u8,
        param_list: &[u8],
        param_list_len: u32,
        nexus: &Nexus,
        lun_present: bool,
    ) -> PrOutOutcome {
        if !lun_present {
            return PrOutOutcome::LuNotSupported;
        }

        // SCOPE must be LU_SCOPE (0x00) for the SAs we support.
        // REGISTER / REGISTER AND IGNORE EXISTING KEY ignore SCOPE
        // and TYPE per SPC-4 §6.14.1; everything else requires
        // LU_SCOPE.
        if scope != 0x00 && service_action != 0x00 && service_action != 0x06 {
            return PrOutOutcome::InvalidFieldInCdb;
        }

        // All supported PROUT service actions take a 24-byte baseline
        // parameter list. REGISTER AND MOVE (0x07) takes a longer
        // one; we don't support it.
        if param_list_len != 24 || param_list.len() < 24 {
            return PrOutOutcome::InvalidFieldInParameterList;
        }
        let p = &param_list[..24];
        let reservation_key = u64::from_be_bytes(p[0..8].try_into().expect("8 bytes"));
        let service_action_key = u64::from_be_bytes(p[8..16].try_into().expect("8 bytes"));
        let aptpl = (p[20] & 0x01) != 0;
        let spec_i_pt = (p[20] & 0x08) != 0;
        let all_tg_pt = (p[20] & 0x04) != 0;

        // We don't support multi-port / SPEC_I_PT / APTPL — every
        // initiator we care about (Windows Failover Cluster, VMware,
        // fence_scsi, clustered backup) sets APTPL=0 and doesn't use
        // SPEC_I_PT. Surface the truthful error so a host that does
        // request these features doesn't proceed believing they were
        // honored.
        if aptpl || spec_i_pt || all_tg_pt {
            return PrOutOutcome::InvalidFieldInParameterList;
        }

        // Dispatch to the shared semantic ops (which lock + mutate
        // per-LUN state). These are the single code path — the NVMe
        // adapter drives the same ops with its own parsed fields.
        match service_action {
            0x00 => self.register(lun, nexus, reservation_key, service_action_key, false),
            0x01 => self.reserve(lun, nexus, reservation_key, type_byte),
            0x02 => self.release(lun, nexus, reservation_key, type_byte),
            0x03 => self.clear(lun, nexus, reservation_key),
            // PREEMPT AND ABORT (0x05) collapses to PREEMPT — we have
            // no task-manager hook, and the visible state transition
            // is identical.
            0x04 | 0x05 => self.preempt(lun, nexus, reservation_key, service_action_key, type_byte),
            0x06 => self.register(lun, nexus, reservation_key, service_action_key, true),
            // REGISTER AND MOVE (0x07) — single target port, rejected.
            _ => PrOutOutcome::InvalidFieldInCdb,
        }
    }

    // ------------------------------------------------------------
    // Semantic reservation ops (transport-neutral)
    // ------------------------------------------------------------
    //
    // Each locks per-LUN state, applies one service action, and
    // returns the response-neutral outcome. `prout` (SCSI) parses its
    // CDB + parameter list then calls these; the NVMe adapter parses
    // its CDW10 + key payload then calls the same ops with an
    // `RegistrantId::NvmeHost`. One mutation path, so the two surfaces
    // can't drift. `lun` is whatever stable per-entity key the caller
    // chooses (SCSI LUN, or `nsid as u64` on the NVMe side).

    /// REGISTER (`sark != 0`) / unregister (`sark == 0`). `ignore`
    /// skips the existing-key check (SCSI REGISTER AND IGNORE EXISTING
    /// KEY / NVMe IEKEY).
    pub fn register(
        &self,
        lun: u64,
        id: &RegistrantId,
        rk: u64,
        sark: u64,
        ignore: bool,
    ) -> PrOutOutcome {
        let mut map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
        prout_register(map.entry(lun).or_default(), id, rk, sark, ignore)
    }

    /// RESERVE / NVMe Acquire (Acquire action).
    pub fn reserve(&self, lun: u64, id: &RegistrantId, rk: u64, type_byte: u8) -> PrOutOutcome {
        let mut map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
        prout_reserve(map.entry(lun).or_default(), id, rk, type_byte)
    }

    /// RELEASE / NVMe Release (Release action).
    pub fn release(&self, lun: u64, id: &RegistrantId, rk: u64, type_byte: u8) -> PrOutOutcome {
        let mut map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
        prout_release(map.entry(lun).or_default(), id, rk, type_byte)
    }

    /// CLEAR / NVMe Release (Clear action).
    pub fn clear(&self, lun: u64, id: &RegistrantId, rk: u64) -> PrOutOutcome {
        let mut map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
        prout_clear(map.entry(lun).or_default(), id, rk)
    }

    /// PREEMPT / PREEMPT AND ABORT / NVMe Acquire (Preempt action).
    pub fn preempt(
        &self,
        lun: u64,
        id: &RegistrantId,
        rk: u64,
        sark: u64,
        type_byte: u8,
    ) -> PrOutOutcome {
        let mut map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
        prout_preempt(map.entry(lun).or_default(), id, rk, sark, type_byte)
    }

    /// Read-only snapshot for a transport-specific reservation
    /// renderer (e.g. the NVMe Reservation Report). Returns an empty
    /// snapshot for a LUN with no state.
    pub fn snapshot(&self, lun: u64) -> ReservationSnapshot {
        let map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
        let Some(state) = map.get(&lun) else {
            return ReservationSnapshot {
                generation: 0,
                reservation_type: None,
                holder: None,
                registrants: Vec::new(),
            };
        };
        ReservationSnapshot {
            generation: state.generation,
            reservation_type: state.reservation.as_ref().map(|r| r.r#type),
            holder: state.reservation.as_ref().map(|r| r.holder.clone()),
            registrants: state
                .registrations
                .iter()
                .map(|(id, key)| (id.clone(), *key))
                .collect(),
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
    out.push(r.r#type.as_u8()); // SCOPE (LU_SCOPE = 0) | TYPE
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
    //   PTPL_A    = 0
    // TYPE_MASK exposes WR_EX, EX_AC, WR_EX_RO, EX_AC_RO, WR_EX_AR,
    // EX_AC_AR — every type the ReservationType enum honors.
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
    for (id, key) in &state.registrations {
        let is_holder =
            holder_key.as_ref() == Some(id) || (res_type.is_some_and(|t| t.is_all_registrants()));
        // Only the iSCSI variant carries an IQN for the TransportID;
        // NVMe registrants never reach the SCSI PRIN path.
        let iqn = match id {
            RegistrantId::Iscsi { iqn, .. } => iqn.as_deref(),
            RegistrantId::NvmeHost { .. } => None,
        };
        descs.extend(full_status_descriptor(*key, is_holder, res_type, iqn));
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
        res_type.map(|t| t.as_u8()).unwrap_or(0)
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
) -> PrOutOutcome {
    if !ignore {
        let current = state.registration_key(nexus).unwrap_or(0);
        if current != rk {
            return PrOutOutcome::ReservationConflict;
        }
    }
    if sark == 0 {
        state.registrations.remove(nexus);
        // Unregistration with the holder of a non-AR reservation
        // releases the reservation (SBC-3 §5.13.4.2).
        if let Some(r) = &state.reservation
            && &r.holder == nexus
        {
            if r.r#type.is_all_registrants() {
                if state.registrations.is_empty() {
                    state.reservation = None;
                } else if let Some((id, key)) = state.registrations.iter().next() {
                    let new_holder = id.clone();
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
        state.registrations.insert(nexus.clone(), sark);
    }
    state.bump_generation();
    PrOutOutcome::Good
}

/// RESERVE (0x01). Idempotent: re-RESERVE with the same nexus +
/// type + scope is a no-op success; conflicting RESERVE is
/// RESERVATION CONFLICT.
fn prout_reserve(state: &mut LunState, nexus: &Nexus, rk: u64, type_byte: u8) -> PrOutOutcome {
    let Some(reg_key) = state.registration_key(nexus) else {
        return PrOutOutcome::ReservationConflict;
    };
    if reg_key != rk {
        return PrOutOutcome::ReservationConflict;
    }
    let Some(r#type) = ReservationType::from_u8(type_byte) else {
        return PrOutOutcome::InvalidFieldInCdb;
    };
    if let Some(existing) = &state.reservation {
        if existing.r#type == r#type && existing.holder == *nexus {
            return PrOutOutcome::Good; // idempotent
        }
        return PrOutOutcome::ReservationConflict;
    }
    state.reservation = Some(ReservationState {
        holder: nexus.clone(),
        key: rk,
        r#type,
    });
    state.bump_generation();
    PrOutOutcome::Good
}

/// RELEASE (0x02). SBC-3 §6.14.4 — silent success when called by a
/// non-holder; conflict if the calling nexus isn't even
/// registered or supplied a stale key; conflict if the TYPE
/// supplied doesn't match the currently-held reservation's TYPE.
fn prout_release(state: &mut LunState, nexus: &Nexus, rk: u64, type_byte: u8) -> PrOutOutcome {
    let Some(reg_key) = state.registration_key(nexus) else {
        return PrOutOutcome::ReservationConflict;
    };
    if reg_key != rk {
        return PrOutOutcome::ReservationConflict;
    }
    let Some(r#type) = ReservationType::from_u8(type_byte) else {
        return PrOutOutcome::InvalidFieldInCdb;
    };
    let Some(existing) = state.reservation.clone() else {
        return PrOutOutcome::Good;
    };
    if existing.r#type != r#type {
        return PrOutOutcome::ReservationConflict;
    }
    let is_holder = existing.holder == *nexus
        || (existing.r#type.is_all_registrants() && state.is_registered(nexus));
    if !is_holder {
        return PrOutOutcome::Good; // not the holder; no-op
    }
    state.reservation = None;
    state.bump_generation();
    PrOutOutcome::Good
}

/// CLEAR (0x03). Wipes every registration and any reservation.
fn prout_clear(state: &mut LunState, nexus: &Nexus, rk: u64) -> PrOutOutcome {
    let Some(reg_key) = state.registration_key(nexus) else {
        return PrOutOutcome::ReservationConflict;
    };
    if reg_key != rk {
        return PrOutOutcome::ReservationConflict;
    }
    state.registrations.clear();
    state.reservation = None;
    state.bump_generation();
    PrOutOutcome::Good
}

/// PREEMPT (0x04) and PREEMPT AND ABORT (0x05). The "abort"
/// variant additionally aborts outstanding tasks for the preempted
/// nexus; we have no task manager hook today so the two collapse to
/// the same handler. The visible state transition is identical.
fn prout_preempt(
    state: &mut LunState,
    nexus: &Nexus,
    rk: u64,
    sark: u64,
    type_byte: u8,
) -> PrOutOutcome {
    let Some(reg_key) = state.registration_key(nexus) else {
        return PrOutOutcome::ReservationConflict;
    };
    if reg_key != rk {
        return PrOutOutcome::ReservationConflict;
    }
    let Some(r#type) = ReservationType::from_u8(type_byte) else {
        return PrOutOutcome::InvalidFieldInCdb;
    };
    // Drop every registration whose key matches SARK *except* the
    // calling nexus (the preemptor remains registered).
    let to_drop: Vec<RegistrantId> = state
        .registrations
        .iter()
        .filter(|(id, k)| **k == sark && **id != *nexus)
        .map(|(id, _)| id.clone())
        .collect();
    if to_drop.is_empty() && state.reservation.as_ref().is_none_or(|r| r.key != sark) {
        // SPC-4: PREEMPT with SARK that matches no registrant and
        // no reservation → RESERVATION CONFLICT.
        return PrOutOutcome::ReservationConflict;
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
    PrOutOutcome::Good
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nexus(tsih: u16, iqn: &str) -> Nexus {
        Nexus::new(tsih, Some(iqn.to_string()))
    }

    fn params(rk: u64, sark: u64, aptpl: bool) -> Vec<u8> {
        let mut p = vec![0u8; 24];
        p[0..8].copy_from_slice(&rk.to_be_bytes());
        p[8..16].copy_from_slice(&sark.to_be_bytes());
        p[20] = if aptpl { 0x01 } else { 0x00 };
        p
    }

    fn register(mgr: &ReservationManager, n: &Nexus, key: u64) {
        // REGISTER AND IGNORE EXISTING KEY (SA 0x06).
        let out = mgr.prout(0, 0x06, 0, 0, &params(0, key, false), 24, n, true);
        assert_eq!(out, PrOutOutcome::Good);
    }

    fn reserve(mgr: &ReservationManager, n: &Nexus, key: u64, type_byte: u8) {
        let out = mgr.prout(0, 0x01, 0, type_byte, &params(key, 0, false), 24, n, true);
        assert_eq!(out, PrOutOutcome::Good);
    }

    fn read_keys(mgr: &ReservationManager) -> Vec<u8> {
        match mgr.prin(0, 0x00, true) {
            PrInOutcome::Good(body) => body,
            other => panic!("expected Good, got {other:?}"),
        }
    }

    #[test]
    fn empty_state_read_keys_returns_zero_keys() {
        let mgr = ReservationManager::new();
        let body = read_keys(&mgr);
        assert_eq!(body.len(), 8);
        assert_eq!(&body[4..8], &0u32.to_be_bytes());
    }

    #[test]
    fn report_capabilities_advertises_six_types() {
        let mgr = ReservationManager::new();
        let body = match mgr.prin(0, 0x02, true) {
            PrInOutcome::Good(b) => b,
            other => panic!("expected Good, got {other:?}"),
        };
        assert_eq!(body.len(), 8);
        assert_eq!(body[1], 0x08);
        assert_eq!(body[3], 0x80); // TMV
        assert_eq!(body[4], 0xEA);
        assert_eq!(body[5], 0x01);
    }

    #[test]
    fn register_then_read_keys_lists_the_key() {
        let mgr = ReservationManager::new();
        let n = nexus(1, "iqn.test:a");
        let out = mgr.prout(0, 0x00, 0, 0, &params(0, 0xDEADBEEF, false), 24, &n, true);
        assert_eq!(out, PrOutOutcome::Good);
        let body = read_keys(&mgr);
        // header(8) + 1 key(8) = 16 bytes
        assert_eq!(body.len(), 16);
        assert_eq!(&body[4..8], &8u32.to_be_bytes());
        assert_eq!(&body[8..16], &0xDEADBEEFu64.to_be_bytes());
    }

    #[test]
    fn reserve_blocks_unregistered_writer() {
        let mgr = ReservationManager::new();
        let na = nexus(1, "iqn.test:a");
        let nb = nexus(2, "iqn.test:b");
        register(&mgr, &na, 0xAAAA);
        reserve(&mgr, &na, 0xAAAA, ReservationType::WriteExclusive.as_u8());
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
        reserve(&mgr, &na, 0xAAAA, ReservationType::ExclusiveAccess.as_u8());
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
            ReservationType::WriteExclusiveRegistrantsOnly.as_u8(),
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
        reserve(&mgr, &na, 0xAAAA, ReservationType::ExclusiveAccess.as_u8());
        let out = mgr.prout(
            0,
            0x02,
            0,
            ReservationType::ExclusiveAccess.as_u8(),
            &params(0xAAAA, 0, false),
            24,
            &na,
            true,
        );
        assert_eq!(out, PrOutOutcome::Good);
        assert!(mgr.allow_write(0, &nexus(2, "iqn.test:b")));
    }

    #[test]
    fn drop_nexus_removes_all_registrations_for_tsih() {
        let mgr = ReservationManager::new();
        let na = nexus(1, "iqn.test:a");
        let nb = nexus(2, "iqn.test:b");
        register(&mgr, &na, 0xAAAA);
        register(&mgr, &nb, 0xBBBB);
        reserve(&mgr, &na, 0xAAAA, ReservationType::ExclusiveAccess.as_u8());
        mgr.drop_nexus(1);
        // A's key is gone; B remains.
        let body = read_keys(&mgr);
        assert_eq!(body.len(), 16);
        assert_eq!(&body[8..16], &0xBBBBu64.to_be_bytes());
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
        reserve(&mgr, &na, 0xAAAA, ReservationType::ExclusiveAccess.as_u8());
        // B preempts A.
        let out = mgr.prout(
            0,
            0x04,
            0,
            ReservationType::WriteExclusive.as_u8(),
            &params(0xBBBB, 0xAAAA, false),
            24,
            &nb,
            true,
        );
        assert_eq!(out, PrOutOutcome::Good);
        // A is no longer registered; A's writes are now blocked.
        assert!(!mgr.allow_write(0, &na));
        assert!(mgr.allow_write(0, &nb));
    }

    #[test]
    fn prout_against_absent_lun_is_lu_not_supported() {
        let mgr = ReservationManager::new();
        let n = nexus(1, "iqn.test:a");
        let out = mgr.prout(0, 0x06, 0, 0, &params(0, 0xAAAA, false), 24, &n, false);
        assert_eq!(out, PrOutOutcome::LuNotSupported);
    }

    #[test]
    fn prout_aptpl_rejected_as_invalid_param_list() {
        let mgr = ReservationManager::new();
        let n = nexus(1, "iqn.test:a");
        let out = mgr.prout(0, 0x06, 0, 0, &params(0, 0xAAAA, true), 24, &n, true);
        assert_eq!(out, PrOutOutcome::InvalidFieldInParameterList);
    }

    #[test]
    fn prout_short_param_list_rejected() {
        let mgr = ReservationManager::new();
        let n = nexus(1, "iqn.test:a");
        let out = mgr.prout(0, 0x06, 0, 0, &[0u8; 8], 24, &n, true);
        assert_eq!(out, PrOutOutcome::InvalidFieldInParameterList);
    }

    #[test]
    fn prin_unknown_service_action_is_invalid_cdb() {
        let mgr = ReservationManager::new();
        assert_eq!(mgr.prin(0, 0x07, true), PrInOutcome::InvalidFieldInCdb);
    }

    #[test]
    fn cdb_slicers_extract_fields() {
        let mut prout = vec![0u8; 10];
        prout[0] = 0x5F;
        prout[1] = 0x01; // RESERVE
        prout[2] = 0x03; // scope 0, type 3
        prout[5..9].copy_from_slice(&24u32.to_be_bytes());
        let f = parse_prout_cdb(&prout, &[1, 2, 3]).expect("parsed");
        assert_eq!(f.service_action, 0x01);
        assert_eq!(f.scope, 0x00);
        assert_eq!(f.type_byte, 0x03);
        assert_eq!(f.param_list_len, 24);
        assert_eq!(f.param_list, &[1, 2, 3]);
        assert!(parse_prout_cdb(&[0x5F; 4], &[]).is_none());

        let mut prin = vec![0u8; 10];
        prin[0] = 0x5E;
        prin[1] = 0x02;
        prin[7..9].copy_from_slice(&64u16.to_be_bytes());
        assert_eq!(parse_prin_cdb(&prin), Some((0x02, 64)));
        assert!(parse_prin_cdb(&[0x5E; 4]).is_none());
    }

    // --- NVMe-host registrant lifecycle (semantic ops + snapshot) ---

    fn nvme_host(seed: u8) -> RegistrantId {
        RegistrantId::nvme([seed; 16])
    }

    #[test]
    fn nvme_register_then_snapshot_lists_key() {
        let mgr = ReservationManager::new();
        let a = nvme_host(0xA1);
        assert_eq!(mgr.register(0, &a, 0, 0xAAAA, true), PrOutOutcome::Good);
        let snap = mgr.snapshot(0);
        assert_eq!(snap.registrants, vec![(a, 0xAAAA)]);
        assert!(snap.holder.is_none());
        assert_eq!(snap.generation, 1);
    }

    #[test]
    fn nvme_acquire_blocks_other_host_write_not_read() {
        let mgr = ReservationManager::new();
        let a = nvme_host(0xA1);
        let b = nvme_host(0xB2);
        mgr.register(0, &a, 0, 0xAAAA, true);
        mgr.register(0, &b, 0, 0xBBBB, true);
        assert_eq!(
            mgr.reserve(0, &a, 0xAAAA, ReservationType::WriteExclusive.as_u8()),
            PrOutOutcome::Good
        );
        // Under Write Exclusive: B (a registrant but not holder) is
        // denied writes, allowed reads; A may write.
        assert!(!mgr.allow_write(0, &b));
        assert!(mgr.allow_read(0, &b));
        assert!(mgr.allow_write(0, &a));
    }

    #[test]
    fn nvme_acquire_by_non_registrant_conflicts() {
        let mgr = ReservationManager::new();
        let a = nvme_host(0xA1);
        // Acquire without registering first → Reservation Conflict
        // (NVMe: an unregistered controller may not acquire).
        assert_eq!(
            mgr.reserve(0, &a, 0xAAAA, ReservationType::WriteExclusive.as_u8()),
            PrOutOutcome::ReservationConflict
        );
    }

    #[test]
    fn drop_nexus_leaves_nvme_registration_intact() {
        // The crux of issue #54: a connection teardown
        // (`drop_nexus`) is iSCSI-only. An NVMe host's registration
        // must survive it — NVMe reservations are released only by
        // explicit Release / unregister / preempt.
        let mgr = ReservationManager::new();
        let iscsi = Nexus::new(7, Some("iqn.test:a".into()));
        let host = nvme_host(0xC3);
        mgr.register(0, &iscsi, 0, 0x1111, true);
        mgr.register(0, &host, 0, 0x2222, true);
        mgr.reserve(0, &host, 0x2222, ReservationType::ExclusiveAccess.as_u8());

        mgr.drop_nexus(7); // tear down the iSCSI nexus

        let snap = mgr.snapshot(0);
        // iSCSI registration gone; NVMe registration + its
        // reservation survive.
        assert_eq!(snap.registrants, vec![(host.clone(), 0x2222)]);
        assert_eq!(snap.holder, Some(host.clone()));
        assert!(!mgr.allow_write(0, &Nexus::new(8, None)));
        assert!(mgr.allow_write(0, &host));
    }

    #[test]
    fn nvme_preempt_across_hosts_replaces_holder() {
        let mgr = ReservationManager::new();
        let a = nvme_host(0xA1);
        let b = nvme_host(0xB2);
        mgr.register(0, &a, 0, 0xAAAA, true);
        mgr.register(0, &b, 0, 0xBBBB, true);
        mgr.reserve(0, &a, 0xAAAA, ReservationType::ExclusiveAccess.as_u8());
        // B preempts A's reservation (SARK = A's key).
        assert_eq!(
            mgr.preempt(
                0,
                &b,
                0xBBBB,
                0xAAAA,
                ReservationType::WriteExclusive.as_u8()
            ),
            PrOutOutcome::Good
        );
        // A is unregistered + no longer holder; B holds.
        let snap = mgr.snapshot(0);
        assert_eq!(snap.registrants, vec![(b.clone(), 0xBBBB)]);
        assert_eq!(snap.holder, Some(b.clone()));
        assert!(!mgr.allow_write(0, &a));
        assert!(mgr.allow_write(0, &b));
    }

    #[test]
    fn nvme_release_clears_reservation() {
        let mgr = ReservationManager::new();
        let a = nvme_host(0xA1);
        mgr.register(0, &a, 0, 0xAAAA, true);
        mgr.reserve(0, &a, 0xAAAA, ReservationType::ExclusiveAccess.as_u8());
        assert_eq!(
            mgr.release(0, &a, 0xAAAA, ReservationType::ExclusiveAccess.as_u8()),
            PrOutOutcome::Good
        );
        assert!(mgr.snapshot(0).holder.is_none());
        // Registration persists after release (NVMe Release ≠ unregister).
        assert_eq!(mgr.snapshot(0).registrants, vec![(a, 0xAAAA)]);
    }
}
