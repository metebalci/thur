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
//! - a set of **registrations** keyed by the stable initiator-port
//!   identity (iSCSI: initiator IQN + ISID; NVMe: the 128-bit HOSTID)
//!   — many-to-one between ports and reservation keys is supported
//!   (cooperating MPIO endpoints share a key);
//! - at most one **reservation** naming the holder nexus, the
//!   reservation key, and the type (0x01-0x08);
//! - a `PR_GENERATION` counter (SPC-4 §6.13.1.1) that increments
//!   on every successful PROUT.
//!
//! ## Persistence (PTPL)
//!
//! Registrations and reservations always survive I_T nexus loss
//! (logout / connection drop) — they are keyed by the stable
//! initiator-port identity, not a session handle. *Power-loss* /
//! daemon-restart persistence (SPC-4 PTPL / NVMe CPTPL) is opt-in per
//! logical unit:
//!
//! - A manager built with [`ReservationManager::new`] is in-memory only
//!   ([`ReservationManager::ptpl_capable`] is false); PTPL_C / RESCAP
//!   bit 0 are advertised as not capable and an `APTPL=1` / `CPTPL=set`
//!   request is rejected. This is the unit-test / no-data-dir mode.
//! - A manager built with [`ReservationManager::load_from`] (or
//!   [`ReservationManager::with_persistence`]) writes each LU whose
//!   most-recent REGISTER set `APTPL=1` (SPC-4 §5.12.3 — the most-recent
//!   REGISTER governs the whole LU) to `<data_dir>/reservations.json`.
//!   The durable write (serialize -> fsync file -> rename -> fsync
//!   parent dir) completes *before* the owning PROUT / Reservation
//!   Register is acknowledged GOOD (persist-before-ack); a write failure
//!   surfaces as [`PrOutOutcome::PersistFailed`] rather than a false ack.
//!   At boot the file is rehydrated fail-safe (a corrupt / unparseable /
//!   foreign file logs a warning and starts empty — it never wedges the
//!   daemon). The record is keyed by a stable per-entity UUID resolved
//!   to the current LUN via an [`EntityResolver`], so a reused LUN never
//!   inherits a defunct volume's fence.
//!
//! ## Scope coverage
//!
//! Only LU_SCOPE (0x00) is honored — element / extent scope are
//! historical and Windows / VMware / Linux cluster managers don't
//! exercise them.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::pr::ReservationType;

/// Registrant identity. A registration (and any reservation it
/// holds) is named by the **stable initiator-port identity** of the
/// host that created it — never by an ephemeral session handle:
///
/// - **iSCSI** initiator port: `(iqn, isid)`. The SCSI TransportID for
///   an iSCSI initiator port is the initiator IQN plus the ISID
///   (RFC 7143 / SPC-4 §7.6.4.7). The initiator chooses its ISID and
///   reuses it for session reinstatement, so `(IQN, ISID)` is stable
///   across logout *and* daemon restart, and it distinguishes MPIO
///   paths correctly. The target-assigned TSIH is deliberately **not**
///   part of identity: it is a per-process counter reset to 1 on every
///   daemon start, so keying registrants by it dropped every
///   reservation on restart (issue #57) and forced re-registration
///   after a reconnect (a deviation — SPC-4 persistent reservations
///   survive I_T nexus loss).
/// - **NVMe** host: the 128-bit Host Identifier from the Fabrics
///   Connect data. One NVMe host opens many controller/queue
///   associations (each its own TCP connection) under a single
///   HOSTID, so the registrant is the *host*, not the connection.
///
/// Under both transports a registration persists until an explicit
/// Release / unregister / Preempt / Clear (or, only when APTPL/CPTPL
/// is not set, a daemon restart). Connection teardown never removes it.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum RegistrantId {
    Iscsi { iqn: Option<String>, isid: [u8; 6] },
    NvmeHost { hostid: [u8; 16] },
}

/// Back-compatible name for the SCSI dispatchers (iSCSI block, tape
/// drive / changer LUN), which only ever construct the iSCSI variant.
pub type Nexus = RegistrantId;

impl RegistrantId {
    /// Build an iSCSI initiator-port identity from the login-advertised
    /// IQN + ISID. The TSIH is intentionally not taken — it is not part
    /// of the registrant identity (see the type docs).
    pub fn iscsi(initiator_iqn: Option<String>, isid: [u8; 6]) -> Self {
        Self::Iscsi {
            iqn: initiator_iqn,
            isid,
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
    /// The state mutation succeeded in memory but the durable write to
    /// the persistence file failed (PTPL persist-before-ack). The owning
    /// PROUT / Reservation Register MUST NOT be acknowledged GOOD —
    /// adapters map this to CHECK CONDITION / HARDWARE ERROR (SCSI:
    /// INTERNAL TARGET FAILURE) or NVMe Internal Error.
    PersistFailed,
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
    /// Current Persist Through Power Loss state for this LU (the
    /// most-recent REGISTER's APTPL/CPTPL). Rendered as the NVMe
    /// Reservation Report PTPLS field.
    pub aptpl: bool,
}

/// The reservation mutation that just succeeded. Disambiguates the
/// proactive-notification class a snapshot diff maps to (e.g. a removed
/// registrant means RegistrationPreempted under a Preempt but
/// ReservationPreempted under a Clear). Transport-neutral — it is a
/// manager-API + [`diff_reservation_changes`] input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResvAction {
    Register,
    Acquire,
    Unregister,
    Release,
    Clear,
    Preempt,
}

/// Neutral proactive-notification class. Maps onto the NVMe LID 0x80
/// type byte / FID 0x82 mask bit at the NVMe AER sink boundary, and onto
/// the two iSCSI RESERVATIONS PREEMPTED / RELEASED Unit Attention codes
/// at the SCSI UA sink boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationChangeKind {
    RegistrationPreempted,
    ReservationReleased,
    ReservationPreempted,
}

/// One proactive reservation change to fan out to a single affected
/// registrant. `affected` carries the full [`RegistrantId`] (not a
/// transport-specific host id) so a dual-transport op surfaces the whole
/// mixed iSCSI + NVMe set in one slice; each sink self-filters by
/// transport variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationChange {
    pub lun: u64,
    pub affected: RegistrantId,
    pub kind: ReservationChangeKind,
}

/// A sink for proactive reservation-change notifications. The manager
/// fires every registered observer once per successful, persisted
/// mutating op, with the issuer-excluded affected set (see
/// [`ReservationManager::register_observer`]).
///
/// thurvsa registers two: the NVMe `AerReservationSink` (LID 0x80 + AER)
/// and the iSCSI `IscsiReservationSink` (Unit Attention). thurvtl
/// registers only the iSCSI one. Centralizing the diff here is what makes
/// the notification fire regardless of which transport originated the
/// change (issue #67).
///
/// Implementations MUST NOT call back into the [`ReservationManager`] —
/// the manager fires the hook after releasing its `by_lun` lock, but a
/// re-entrant mutate would still violate the documented lock order.
pub trait ReservationObserver: Send + Sync {
    fn on_reservation_change(&self, changes: &[ReservationChange]);
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

#[derive(Default, Clone)]
struct LunState {
    /// `registrant -> reservation key`. Ordered map so READ KEYS /
    /// READ FULL STATUS render in a stable order across runs —
    /// initiators don't care, but it makes test diffs and audit logs
    /// readable.
    registrations: BTreeMap<RegistrantId, u64>,
    reservation: Option<ReservationState>,
    /// SPC-4 §6.13.1.1 PR_GENERATION. Wraps on overflow per spec.
    generation: u32,
    /// APTPL (SCSI) / CPTPL-set (NVMe): when true this LU's PR state is
    /// persisted across power loss / daemon restart. Set by the
    /// most-recent REGISTER (SPC-4 §5.12.3 — the most-recent REGISTER's
    /// APTPL governs the entire logical unit). Does not affect
    /// persistence across nexus loss, which is unconditional.
    aptpl: bool,
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

    /// A LU is written to the persistence file iff its host asked for
    /// power-loss persistence (APTPL/CPTPL) AND it has live state.
    /// Omitting an empty LU (post-CLEAR) or an `aptpl=false` LU from the
    /// rewrite is exactly the "delete the on-disk record on CLEAR / on
    /// APTPL->0" behavior (SPC-4 §5.12.3 / NVMe CPTPL clear).
    fn persist_eligible(&self) -> bool {
        self.aptpl && (!self.registrations.is_empty() || self.reservation.is_some())
    }
}

/// Maps the manager's runtime per-entity key (`lun`) to a stable,
/// restart-invariant identity (a 16-byte UUID) and back. The
/// persistence file is keyed by the UUID, not the LUN, because a LUN
/// number is *not* stable: thurvsa reclaims the lowest free LUN on
/// volume delete, so persisting by LUN would let a deleted volume's
/// fence land on whatever new volume reused its number. The resolver
/// lets the (product-neutral) manager translate without knowing about
/// volumes or the chassis. thurvsa implements it over its
/// `VolumeRegistry` (manifest UUID); thurvtl uses [`LunIdentity`]
/// because its drive / changer LUNs are themselves the stable identity.
pub trait EntityResolver: Send + Sync {
    /// Stable UUID for a live LUN, or `None` if the LUN is unknown
    /// (e.g. its volume was deleted) — such records are not persisted.
    fn uuid_for_lun(&self, lun: u64) -> Option<[u8; 16]>;
    /// Current LUN for a persisted UUID, or `None` if the entity no
    /// longer exists — its persisted record is then dropped at load.
    fn lun_for_uuid(&self, uuid: &[u8; 16]) -> Option<u64>;
}

/// [`EntityResolver`] for targets where the LUN itself is the stable
/// identity (thurvtl's fixed drive / changer LUNs, declared by the
/// chassis topology and reconciled at every start). The UUID is just
/// the LUN in the low 8 bytes, big-endian, with the high 8 bytes zero.
pub struct LunIdentity;

impl EntityResolver for LunIdentity {
    fn uuid_for_lun(&self, lun: u64) -> Option<[u8; 16]> {
        let mut u = [0u8; 16];
        u[8..16].copy_from_slice(&lun.to_be_bytes());
        Some(u)
    }
    fn lun_for_uuid(&self, uuid: &[u8; 16]) -> Option<u64> {
        if uuid[0..8] != [0u8; 8] {
            return None;
        }
        Some(u64::from_be_bytes(uuid[8..16].try_into().expect("8 bytes")))
    }
}

/// Power-loss persistence wiring. Present iff the manager was built
/// with a data dir; absent for the in-memory `new()` mode.
struct Persist {
    path: PathBuf,
    resolver: Arc<dyn EntityResolver>,
}

/// Per-LUN registration / reservation state, mediated by a single
/// mutex. Reservation traffic is rare relative to the data path (one
/// or two PROUTs per host boot + a quick PRIN sweep); finer-grained
/// locking would just be ceremony.
pub struct ReservationManager {
    by_lun: Mutex<BTreeMap<u64, LunState>>,
    /// Serializes mutations (PROUT / Reservation Register) end-to-end,
    /// including the durable persist, so the `by_lun` data-path lock is
    /// NOT held during the fsync chain (issue #181). `allow_read` /
    /// `allow_write` — one per data-path IO on both transports — only
    /// take `by_lun`, so they no longer stall behind a peer's
    /// APTPL-persist fsync (which used to run under `by_lun`, freezing
    /// IO admission on every LUN of the daemon). Mutations stay
    /// serialized against each other, so persist-before-ack rollback has
    /// no concurrent mutation to race and the file write can't tear.
    mutate_lock: Mutex<()>,
    /// `None` = in-memory only (PTPL not capable). `Some` = persist
    /// APTPL/CPTPL-set LUs to disk (persist-before-ack).
    persist: Option<Persist>,
    /// Proactive-notification sinks (issue #67), empty by default.
    /// Registered at daemon boot via [`Self::register_observer`] — the
    /// sinks' dependencies (NVMe aer hub, iSCSI UA tracker) are born
    /// later than the manager, so registration is runtime, not
    /// construction-time. Boot is single-threaded, so there is never a
    /// concurrent register / fire.
    observers: Mutex<Vec<Arc<dyn ReservationObserver>>>,
}

impl Default for ReservationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ReservationManager {
    /// In-memory only: no power-loss persistence (PTPL advertised as not
    /// capable; `APTPL=1` / `CPTPL=set` rejected). Used by unit tests and
    /// any caller without a data dir.
    pub fn new() -> Self {
        Self {
            by_lun: Mutex::new(BTreeMap::new()),
            mutate_lock: Mutex::new(()),
            persist: None,
            observers: Mutex::new(Vec::new()),
        }
    }

    /// Persistence-enabled but starting from empty state. Mutations to
    /// an APTPL=1 LU are written to `path` (atomic + parent-dir fsync)
    /// before their PROUT / Reservation Register is acknowledged.
    /// Prefer [`Self::load_from`], which also rehydrates an existing
    /// file at boot.
    pub fn with_persistence(path: PathBuf, resolver: Arc<dyn EntityResolver>) -> Self {
        Self {
            by_lun: Mutex::new(BTreeMap::new()),
            mutate_lock: Mutex::new(()),
            persist: Some(Persist { path, resolver }),
            observers: Mutex::new(Vec::new()),
        }
    }

    /// Persistence-enabled, rehydrating any existing on-disk state.
    /// Fail-safe: a missing file starts empty (first boot); a truncated
    /// / unparseable / unknown-version file logs a warning and starts
    /// empty (never panics, never blocks boot). Each persisted record is
    /// resolved UUID -> current LUN via `resolver`; a record whose entity
    /// no longer exists is dropped (closes the LUN-reuse fencing hole).
    pub fn load_from(path: PathBuf, resolver: Arc<dyn EntityResolver>) -> Self {
        let by_lun = load_file(&path, resolver.as_ref());
        Self {
            by_lun: Mutex::new(by_lun),
            mutate_lock: Mutex::new(()),
            persist: Some(Persist { path, resolver }),
            observers: Mutex::new(Vec::new()),
        }
    }

    /// Register a proactive-notification sink (issue #67). Called at
    /// daemon boot, before any transport listener accepts a connection,
    /// so it never races [`mutate`]'s fan-out. A manager with no
    /// observers (every unit test, any caller that skips this) does no
    /// extra work on the mutate hot path.
    pub fn register_observer(&self, observer: Arc<dyn ReservationObserver>) {
        self.observers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(observer);
    }

    /// True when power-loss persistence is wired (a data dir is present).
    /// Gates the PTPL_C (SCSI) / RESCAP bit 0 (NVMe) advertisement and
    /// the `APTPL=1` / `CPTPL=set` accept so the advertised capability
    /// can never diverge from the actual behavior.
    pub fn ptpl_capable(&self) -> bool {
        self.persist.is_some()
    }

    /// Drop a single LUN's reservation state, in memory and on disk.
    /// Called when a volume is deleted (thurvsa `volume destroy`): it
    /// removes the in-memory entry so a later volume that reuses this
    /// LUN number can't inherit the gone volume's fence (the live
    /// counterpart of the load-time UUID validation), and rewrites the
    /// persistence file without it. No-op when the LUN has no state.
    pub fn purge_lun(&self, lun: u64) {
        // Serialize against `mutate` and snapshot under `by_lun`, then
        // rewrite the file off the data-path lock (issue #181) — same
        // shape as `mutate`. Best-effort: no rollback (the volume is
        // already destroyed, and a stale persisted record is dropped at
        // load by the UUID resolver), so a failed rewrite just warns.
        let _mutate_guard = self.mutate_lock.lock().unwrap_or_else(|p| p.into_inner());
        let snapshot = {
            let mut map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
            if map.remove(&lun).is_none() {
                return;
            }
            self.persist.is_some().then(|| map.clone())
        };
        if let Some(snapshot) = snapshot.as_ref()
            && let Some(persist) = self.persist.as_ref()
            && let Err(e) = persist_to_disk(snapshot, persist)
        {
            warn!("reservations: rewrite after purging LUN {lun} failed ({e})");
        }
    }

    /// Run one state mutation under the lock with persist-before-ack
    /// semantics. On a successful (`Good`) mutation that changes the
    /// persisted set (the LU was or becomes persist-eligible), the file
    /// is rewritten and fsynced before returning; if that durable write
    /// fails the **in-memory mutation is rolled back** and the call maps
    /// to [`PrOutOutcome::PersistFailed`], so the state matches what the
    /// host was told (persist-before-ack: a fence the host believes
    /// failed must not silently linger and fence its peers, nor would it
    /// survive a restart). Non-`Good` outcomes never touch disk (the
    /// handlers mutate nothing on conflict).
    fn mutate<F>(&self, lun: u64, action: ResvAction, issuer: &RegistrantId, f: F) -> PrOutOutcome
    where
        F: FnOnce(&mut LunState) -> PrOutOutcome,
    {
        // Serialize mutations (PROUT / Reservation Register) end-to-end,
        // INCLUDING the off-`by_lun` durable persist below (issue #181).
        // The data path's `allow_read` / `allow_write` take only `by_lun`,
        // so releasing it before the fsync chain stops one APTPL-persist
        // from freezing IO admission on every LUN of the daemon; holding
        // `mutate_lock` across the persist keeps mutations serialized, so
        // the persist-before-ack rollback has no concurrent mutation to
        // race and the temp-file write can't tear.
        let _mutate_guard = self.mutate_lock.lock().unwrap_or_else(|p| p.into_inner());

        // Apply the in-memory mutation under `by_lun`, snapshot everything
        // the post-mutation steps need, then release `by_lun` before the
        // fsync. A non-`Good` outcome touches nothing on disk and returns
        // immediately. `prior` is the pre-mutation entry, captured so a
        // failed durable write can be rolled back; `mutate_lock` makes
        // that rollback unambiguous (no other mutation could have run).
        let (prior, persist_snapshot, observers, pre, post) = {
            let mut map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
            let was_eligible = map.get(&lun).is_some_and(LunState::persist_eligible);
            let prior = map.get(&lun).cloned();
            let outcome = f(map.entry(lun).or_default());
            if outcome != PrOutOutcome::Good {
                return outcome;
            }
            let now_eligible = map.get(&lun).is_some_and(LunState::persist_eligible);
            // Clone the full persist set under the lock; `mutate_lock`
            // guarantees it's still current when persist_to_disk runs.
            let persist_snapshot = (self.persist.is_some() && (was_eligible || now_eligible))
                .then(|| map.clone());
            // Empty-observer fast path keeps the hot path clone-free.
            let observers = {
                let obs = self.observers.lock().unwrap_or_else(|p| p.into_inner());
                if obs.is_empty() {
                    None
                } else {
                    Some(obs.clone())
                }
            };
            let pre = snapshot_of(prior.as_ref());
            let post = snapshot_of(map.get(&lun));
            (prior, persist_snapshot, observers, pre, post)
        };

        // Durable persist OFF the `by_lun` lock. On failure, roll the
        // in-memory mutation back (persist-before-ack: a fence the host
        // believes failed must not silently linger or survive a restart).
        if let Some(snapshot) = persist_snapshot.as_ref()
            && let Some(persist) = self.persist.as_ref()
            && let Err(e) = persist_to_disk(snapshot, persist)
        {
            warn!(
                "reservations: durable persist to {} failed ({e}); rolling back the mutation",
                persist.path.display()
            );
            let mut map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
            match prior {
                Some(state) => {
                    map.insert(lun, state);
                }
                None => {
                    map.remove(&lun);
                }
            }
            return PrOutOutcome::PersistFailed;
        }

        // Proactive cross-transport notification (issue #67), off every
        // lock — the sinks take other locks (NVMe aer hub, iSCSI UA
        // tracker / session table) and must never run while `by_lun` is
        // held.
        if let Some(observers) = observers {
            let changes = diff_reservation_changes(action, lun, issuer, &pre, &post);
            if !changes.is_empty() {
                for observer in &observers {
                    observer.on_reservation_change(&changes);
                }
            }
        }
        PrOutOutcome::Good
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
            // PTPL_C reflects whether this manager can persist at all;
            // PTPL_A reflects this LU's currently-active APTPL bit.
            0x02 => render_report_capabilities(self.ptpl_capable(), state.is_some_and(|s| s.aptpl)),
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

        // SPEC_I_PT and ALL_TG_PT remain unsupported (single target
        // port, no multi-port registration) — reject them truthfully.
        // APTPL is honored when persistence is wired; reject APTPL=1
        // only when this manager cannot actually persist, so the
        // advertised PTPL_C and the behavior can never diverge.
        if spec_i_pt || all_tg_pt {
            return PrOutOutcome::InvalidFieldInParameterList;
        }
        if aptpl && !self.ptpl_capable() {
            return PrOutOutcome::InvalidFieldInParameterList;
        }

        // Dispatch to the shared semantic ops (which lock + mutate
        // per-LUN state). These are the single code path — the NVMe
        // adapter drives the same ops with its own parsed fields. The
        // APTPL bit is meaningful only for REGISTER / REGISTER AND
        // IGNORE EXISTING KEY (SPC-4 §6.14.3); other SAs leave the LU's
        // persist setting untouched.
        match service_action {
            0x00 => self.register(
                lun,
                nexus,
                reservation_key,
                service_action_key,
                false,
                Some(aptpl),
            ),
            0x01 => self.reserve(lun, nexus, reservation_key, type_byte),
            0x02 => self.release(lun, nexus, reservation_key, type_byte),
            0x03 => self.clear(lun, nexus, reservation_key),
            // PREEMPT AND ABORT (0x05) collapses to PREEMPT — we have
            // no task-manager hook, and the visible state transition
            // is identical.
            0x04 | 0x05 => self.preempt(lun, nexus, reservation_key, service_action_key, type_byte),
            0x06 => self.register(
                lun,
                nexus,
                reservation_key,
                service_action_key,
                true,
                Some(aptpl),
            ),
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
    /// KEY / NVMe IEKEY). `aptpl` sets the LU's power-loss-persist bit:
    /// `Some(v)` applies `v` (SCSI always supplies the bit; NVMe maps
    /// CPTPL set/clear), `None` leaves it unchanged (NVMe CPTPL=no-change).
    pub fn register(
        &self,
        lun: u64,
        id: &RegistrantId,
        rk: u64,
        sark: u64,
        ignore: bool,
        aptpl: Option<bool>,
    ) -> PrOutOutcome {
        // A REGISTER with service-action-key 0 unregisters this port
        // (SPC-4 §6.14.3); only the unregister case can release a held
        // reservation, so it carries the notification-bearing action.
        let action = if sark == 0 {
            ResvAction::Unregister
        } else {
            ResvAction::Register
        };
        self.mutate(lun, action, id, |st| {
            prout_register(st, id, rk, sark, ignore, aptpl)
        })
    }

    /// RESERVE / NVMe Acquire (Acquire action).
    pub fn reserve(&self, lun: u64, id: &RegistrantId, rk: u64, type_byte: u8) -> PrOutOutcome {
        self.mutate(lun, ResvAction::Acquire, id, |st| {
            prout_reserve(st, id, rk, type_byte)
        })
    }

    /// RELEASE / NVMe Release (Release action).
    pub fn release(&self, lun: u64, id: &RegistrantId, rk: u64, type_byte: u8) -> PrOutOutcome {
        self.mutate(lun, ResvAction::Release, id, |st| {
            prout_release(st, id, rk, type_byte)
        })
    }

    /// CLEAR / NVMe Release (Clear action).
    pub fn clear(&self, lun: u64, id: &RegistrantId, rk: u64) -> PrOutOutcome {
        self.mutate(lun, ResvAction::Clear, id, |st| prout_clear(st, id, rk))
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
        self.mutate(lun, ResvAction::Preempt, id, |st| {
            prout_preempt(st, id, rk, sark, type_byte)
        })
    }

    /// Read-only snapshot for a transport-specific reservation
    /// renderer (e.g. the NVMe Reservation Report). Returns an empty
    /// snapshot for a LUN with no state.
    pub fn snapshot(&self, lun: u64) -> ReservationSnapshot {
        let map = self.by_lun.lock().unwrap_or_else(|p| p.into_inner());
        snapshot_of(map.get(&lun))
    }
}

/// Build a [`ReservationSnapshot`] from an already-borrowed [`LunState`]
/// (or its absence). Shared by the public [`ReservationManager::snapshot`]
/// and by `mutate`'s notification hook, which holds the `by_lun` lock and
/// so cannot re-lock through `snapshot`.
fn snapshot_of(state: Option<&LunState>) -> ReservationSnapshot {
    let Some(state) = state else {
        return ReservationSnapshot {
            generation: 0,
            reservation_type: None,
            holder: None,
            registrants: Vec::new(),
            aptpl: false,
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
        aptpl: state.aptpl,
    }
}

/// Derive the proactive reservation-change notifications a just-completed
/// (`Good`) mutating op generates, from the before/after snapshots, the
/// issuing registrant, and the action. Pure and table-testable.
///
/// Transport-neutral: it emits a [`ReservationChange`] for **every**
/// affected registrant of either transport (the NVMe AER sink and the
/// iSCSI UA sink each self-filter by variant). The command issuer is
/// excluded here, once, by [`RegistrantId`] equality — handling an iSCSI
/// issuer and an NVMe issuer uniformly — so a host never receives an
/// asynchronous notice for its own command.
///
/// Rules (mirroring the NVM Command Set reservation notices, now applied
/// to both transports):
/// - **Preempt**: the prior reservation holder that lost its reservation
///   → `ReservationPreempted`; any other registrant whose registration
///   was removed → `RegistrationPreempted` (the holder is deduped to
///   `ReservationPreempted` only, never both).
/// - **Release / Unregister**: a reservation that existed and is now
///   released with no successor → `ReservationReleased` to every other
///   current registrant. An all-registrants holder *rotation* keeps
///   `post.holder` set, so it emits nothing.
/// - **Clear**: every registration and the reservation are wiped →
///   `ReservationPreempted` to every other prior registrant.
/// - **Acquire / Register**: never fence another registrant → nothing.
pub fn diff_reservation_changes(
    action: ResvAction,
    lun: u64,
    issuer: &RegistrantId,
    pre: &ReservationSnapshot,
    post: &ReservationSnapshot,
) -> Vec<ReservationChange> {
    let mut out = Vec::new();
    // The command issuer learns the result from its own completion; it is
    // never sent an asynchronous notification.
    let mut emit = |affected: &RegistrantId, kind: ReservationChangeKind| {
        if affected != issuer {
            out.push(ReservationChange {
                lun,
                affected: affected.clone(),
                kind,
            });
        }
    };
    let is_present = |id: &RegistrantId| post.registrants.iter().any(|(p, _)| p == id);

    match action {
        ResvAction::Register | ResvAction::Acquire => {}
        ResvAction::Clear => {
            for (id, _) in &pre.registrants {
                emit(id, ReservationChangeKind::ReservationPreempted);
            }
        }
        ResvAction::Preempt => {
            // The prior holder whose reservation was taken over.
            let reservation_taken = pre.holder.is_some() && post.holder != pre.holder;
            let preempted_holder = if reservation_taken {
                pre.holder.as_ref()
            } else {
                None
            };
            if let Some(h) = preempted_holder {
                emit(h, ReservationChangeKind::ReservationPreempted);
            }
            // Registrants removed by the preempt that were not the
            // (already-notified) holder.
            for (id, _) in &pre.registrants {
                if is_present(id) {
                    continue;
                }
                if Some(id) == preempted_holder {
                    continue;
                }
                emit(id, ReservationChangeKind::RegistrationPreempted);
            }
        }
        ResvAction::Release | ResvAction::Unregister => {
            let released = pre.holder.is_some() && post.holder.is_none();
            if released {
                for (id, _) in &post.registrants {
                    emit(id, ReservationChangeKind::ReservationReleased);
                }
            }
        }
    }
    out
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
    // SPC-4 6.13.3.2: the RESERVATION KEY is zero for All-Registrants
    // reservation types — the reservation is held collectively, not by
    // one key. Emitting the creating registrant's key makes spec-correct
    // hosts (sg_persist, fencing agents) treat it as key-held and target
    // the wrong key in PREEMPT/failover (issue #182/#79).
    let report_key = if r.r#type.is_all_registrants() {
        0u64
    } else {
        r.key
    };
    out.extend_from_slice(&report_key.to_be_bytes());
    out.extend_from_slice(&[0u8; 4]); // obsolete (scope-specific addr)
    out.push(0); // reserved
    out.push(r.r#type.as_u8()); // SCOPE (LU_SCOPE = 0) | TYPE
    out.extend_from_slice(&[0u8; 2]); // obsolete
    out
}

fn render_report_capabilities(ptpl_capable: bool, ptpl_active: bool) -> Vec<u8> {
    // SPC-4 Table 86 — REPORT CAPABILITIES parameter data.
    // 8 bytes total. We declare:
    //   PTPL_C    = ptpl_capable (persist-through-power-loss capable;
    //               true iff a data dir is wired — issue #57)
    //   ATP_C     = 0  (ALL_TG_PT not supported)
    //   SIP_C     = 0  (SPEC_I_PT not supported)
    //   CRH       = 0  (no compatible-reservation handling for legacy
    //                   RESERVE(6) / RELEASE(6))
    //   TMV       = 1  (TYPE_MASK valid)
    //   PTPL_A    = ptpl_active (this LU's currently-active APTPL bit)
    // TYPE_MASK exposes WR_EX, EX_AC, WR_EX_RO, EX_AC_RO, WR_EX_AR,
    // EX_AC_AR — every type the ReservationType enum honors.
    let mut buf = vec![0u8; 8];
    buf[0] = 0x00;
    buf[1] = 0x08; // length = 8 bytes total minus the leading 0
    buf[2] = if ptpl_capable { 0x01 } else { 0x00 }; // bit0 PTPL_C
    buf[3] = 0x80 | if ptpl_active { 0x01 } else { 0x00 }; // bit7 TMV | bit0 PTPL_A
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
        // Only the iSCSI variant carries an IQN for the TransportID.
        // Under a dual-transport export (issue #66) an NVMe host can be
        // a registrant on the same LUN a SCSI initiator is reading FULL
        // STATUS for: there is no clean SPC-4 TransportID format for an
        // NVMe host, so we render the descriptor with an empty iSCSI
        // TransportID. The key, R_HOLDER bit, and type stay correct, so
        // `sg_persist --read-full-status` still reports the holder; only
        // the registrant's transport name is blank (documented
        // limitation). READ RESERVATION / READ KEYS are unaffected.
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
    aptpl: Option<bool>,
) -> PrOutOutcome {
    if !ignore {
        let current = state.registration_key(nexus).unwrap_or(0);
        if current != rk {
            return PrOutOutcome::ReservationConflict;
        }
    }
    // SPC-4 §5.12.3: the APTPL of the most-recent REGISTER (register or
    // unregister) governs the whole LU's power-loss persistence.
    if let Some(v) = aptpl {
        state.aptpl = v;
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
        // SPC-4 5.12.6.3: a holder re-issuing RESERVE with the same
        // SCOPE/TYPE gets GOOD, no change. For All-Registrants types
        // (5.12.10) every registered I_T nexus is a holder, so a
        // co-registrant's idempotent RESERVE of the same type must also
        // succeed rather than RESERVATION CONFLICT (issue #182/#80) —
        // mirrors the release path's AR holder check.
        let is_holder = existing.holder == *nexus
            || (existing.r#type.is_all_registrants() && state.is_registered(nexus));
        if existing.r#type == r#type && is_holder {
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
    // SPC-4 5.12.11.2: PREEMPT with SARK=0 identifies an All-Registrants
    // reservation. If one is held, remove every registration except the
    // preemptor's and install the new reservation under TYPE — this is
    // the spec-defined cluster-failover takeover. If SARK=0 and the held
    // reservation is NOT all-registrants (or none is held), it's INVALID
    // FIELD IN PARAMETER LIST, not RESERVATION CONFLICT (issue #182/#81).
    if sark == 0 {
        return match &state.reservation {
            Some(r) if r.r#type.is_all_registrants() => {
                let to_drop: Vec<RegistrantId> = state
                    .registrations
                    .iter()
                    .filter(|(id, _)| **id != *nexus)
                    .map(|(id, _)| id.clone())
                    .collect();
                for k in &to_drop {
                    state.registrations.remove(k);
                }
                state.reservation = Some(ReservationState {
                    holder: nexus.clone(),
                    key: rk,
                    r#type,
                });
                state.bump_generation();
                PrOutOutcome::Good
            }
            _ => PrOutOutcome::InvalidFieldInParameterList,
        };
    }
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

// ----------------------------------------------------------------
// Persistence (PTPL) — on-disk DTO + atomic write + fail-safe load
// ----------------------------------------------------------------

/// `reservations.json` schema version. Bumped only on an incompatible
/// layout change; an unknown version loads as empty (fail-safe).
const PERSIST_VERSION: u32 = 1;

/// Whole-file document. Rewritten in full on every persist-eligible
/// mutation (PR traffic is rare, so a small full rewrite is cheaper
/// than incremental bookkeeping and is trivially crash-consistent).
#[derive(Serialize, Deserialize)]
struct PersistedFile {
    version: u32,
    volumes: Vec<PersistedVolume>,
}

/// One logical unit's persisted state, keyed by its stable UUID
/// (resolved to the current LUN at load via [`EntityResolver`]). Only
/// LUs whose most-recent REGISTER set APTPL=1 appear here.
#[derive(Serialize, Deserialize)]
struct PersistedVolume {
    uuid: [u8; 16],
    aptpl: bool,
    generation: u32,
    registrations: Vec<PersistedReg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reservation: Option<PersistedReservation>,
}

#[derive(Serialize, Deserialize)]
struct PersistedReg {
    id: RegistrantId,
    key: u64,
}

#[derive(Serialize, Deserialize)]
struct PersistedReservation {
    holder: RegistrantId,
    key: u64,
    /// ReservationType as its SPC-4 numeric byte (`as_u8` / `from_u8`).
    /// The numeric wire mapping stays authoritative — we deliberately
    /// don't derive serde on the enum.
    type_byte: u8,
}

impl PersistedVolume {
    fn from_state(uuid: [u8; 16], state: &LunState) -> Self {
        Self {
            uuid,
            aptpl: state.aptpl,
            generation: state.generation,
            registrations: state
                .registrations
                .iter()
                .map(|(id, key)| PersistedReg {
                    id: id.clone(),
                    key: *key,
                })
                .collect(),
            reservation: state.reservation.as_ref().map(|r| PersistedReservation {
                holder: r.holder.clone(),
                key: r.key,
                type_byte: r.r#type.as_u8(),
            }),
        }
    }

    fn into_lun_state(self) -> LunState {
        let registrations = self
            .registrations
            .into_iter()
            .map(|r| (r.id, r.key))
            .collect();
        // A reservation whose persisted type byte no longer maps (only
        // possible from a corrupt / hand-edited file) is dropped — the
        // registrations are still honored. Fail-safe, never panic.
        let reservation = self.reservation.and_then(|r| {
            ReservationType::from_u8(r.type_byte).map(|t| ReservationState {
                holder: r.holder,
                key: r.key,
                r#type: t,
            })
        });
        LunState {
            registrations,
            reservation,
            generation: self.generation,
            aptpl: self.aptpl,
        }
    }
}

/// Rehydrate `path` into the per-LUN map, fail-safe. A missing file is
/// first boot (empty, no warning); any other read / parse / version
/// problem logs a warning and starts empty so a corrupt or foreign file
/// can never wedge the daemon. Records whose UUID no longer resolves to
/// a live LUN are dropped.
fn load_file(path: &Path, resolver: &dyn EntityResolver) -> BTreeMap<u64, LunState> {
    let mut by_lun = BTreeMap::new();
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return by_lun,
        Err(e) => {
            warn!(
                "reservations: cannot read {} ({e}); starting with empty reservation state",
                path.display()
            );
            return by_lun;
        }
    };
    let file: PersistedFile = match serde_json::from_slice(&bytes) {
        Ok(f) => f,
        Err(e) => {
            warn!(
                "reservations: {} is unparseable ({e}); starting with empty reservation state",
                path.display()
            );
            return by_lun;
        }
    };
    if file.version != PERSIST_VERSION {
        warn!(
            "reservations: {} has unsupported version {} (expected {PERSIST_VERSION}); starting empty",
            path.display(),
            file.version
        );
        return by_lun;
    }
    for vol in file.volumes {
        match resolver.lun_for_uuid(&vol.uuid) {
            Some(lun) => {
                by_lun.insert(lun, vol.into_lun_state());
            }
            None => warn!(
                "reservations: dropping persisted record for volume {} (no current LUN)",
                hex16(&vol.uuid)
            ),
        }
    }
    by_lun
}

/// Serialize every persist-eligible LU and write `path` atomically:
/// write a temp file, fsync it, chmod 0640, rename over the target,
/// then fsync the parent directory so the rename itself is durable
/// across power loss. The caller holds the `by_lun` lock.
fn persist_to_disk(map: &BTreeMap<u64, LunState>, persist: &Persist) -> std::io::Result<()> {
    let volumes: Vec<PersistedVolume> = map
        .iter()
        .filter(|(_, st)| st.persist_eligible())
        // Skip LUs the resolver can't map to a UUID (e.g. a synthetic
        // call site) — they simply aren't persisted.
        .filter_map(|(lun, st)| {
            persist
                .resolver
                .uuid_for_lun(*lun)
                .map(|uuid| PersistedVolume::from_state(uuid, st))
        })
        .collect();
    let file = PersistedFile {
        version: PERSIST_VERSION,
        volumes,
    };

    let path = &persist.path;
    let tmp = path.with_extension("json.tmp");
    {
        let f = std::fs::File::create(&tmp)?;
        let mut w = std::io::BufWriter::new(f);
        serde_json::to_writer(&mut w, &file).map_err(std::io::Error::other)?;
        w.flush()?;
        w.into_inner()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o640))?;
    }
    std::fs::rename(&tmp, path)?;
    // Parent-dir fsync — the one true power-loss-durability gap the
    // VolumeManifest recipe doesn't cover. Best-effort: a parent we
    // can't open isn't worth failing the (already-renamed) write over.
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Lowercase hex of a 16-byte UUID for log lines (plain ASCII).
fn hex16(uuid: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in uuid {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build an iSCSI registrant. `tag` seeds a distinct ISID so the
    // two-/three-initiator tests keep distinct registrant identities
    // (the old `tsih` argument played that role before identity moved
    // to (IQN, ISID)).
    fn nexus(tag: u16, iqn: &str) -> Nexus {
        Nexus::iscsi(Some(iqn.to_string()), [tag as u8; 6])
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
        assert_eq!(body[2], 0x00); // PTPL_C = 0 (in-memory manager not capable)
        assert_eq!(body[3], 0x80); // TMV, PTPL_A = 0
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

    /// Issue #182/#79: READ RESERVATION reports a zero key for an
    /// All-Registrants reservation (held collectively, not key-held).
    #[test]
    fn read_reservation_zeroes_key_for_all_registrants() {
        let mgr = ReservationManager::new();
        let a = nexus(1, "iqn.test:a");
        register(&mgr, &a, 0xAAAA);
        reserve(&mgr, &a, 0xAAAA, 0x07); // WR_EX_AR
        let body = match mgr.prin(0, 0x01, true) {
            PrInOutcome::Good(b) => b,
            other => panic!("expected Good, got {other:?}"),
        };
        assert_eq!(body.len(), 24, "header(8) + 16-byte descriptor");
        assert_eq!(
            &body[8..16],
            &0u64.to_be_bytes(),
            "All-Registrants reservation key must be reported as zero"
        );
    }

    /// Issue #182/#80: a co-registrant re-issuing RESERVE of the same
    /// All-Registrants type gets GOOD (idempotent), not CONFLICT.
    #[test]
    fn co_registrant_reserve_all_registrants_is_idempotent() {
        let mgr = ReservationManager::new();
        let a = nexus(1, "iqn.test:a");
        let b = nexus(2, "iqn.test:b");
        register(&mgr, &a, 0xAAAA);
        register(&mgr, &b, 0xBBBB);
        reserve(&mgr, &a, 0xAAAA, 0x07); // a creates WR_EX_AR
        let out = mgr.prout(0, 0x01, 0, 0x07, &params(0xBBBB, 0, false), 24, &b, true);
        assert_eq!(out, PrOutOutcome::Good, "co-registrant RESERVE must be idempotent");
    }

    /// Issue #182/#81: PREEMPT with SARK=0 against an All-Registrants
    /// reservation takes it over (drops other registrants); against a
    /// non-AR reservation it is INVALID FIELD IN PARAMETER LIST.
    #[test]
    fn preempt_sark_zero_takes_over_all_registrants() {
        let mgr = ReservationManager::new();
        let a = nexus(1, "iqn.test:a");
        let b = nexus(2, "iqn.test:b");
        register(&mgr, &a, 0xAAAA);
        register(&mgr, &b, 0xBBBB);
        reserve(&mgr, &a, 0xAAAA, 0x07); // WR_EX_AR
        let out = mgr.prout(0, 0x04, 0, 0x07, &params(0xBBBB, 0, false), 24, &b, true);
        assert_eq!(out, PrOutOutcome::Good, "SARK=0 must take over an AR reservation");
        // a's registration was dropped; only b's key remains.
        let body = read_keys(&mgr);
        assert_eq!(&body[4..8], &8u32.to_be_bytes(), "exactly one key remains");
        assert_eq!(&body[8..16], &0xBBBBu64.to_be_bytes());
    }

    #[test]
    fn preempt_sark_zero_on_non_ar_is_invalid_param() {
        let mgr = ReservationManager::new();
        let a = nexus(1, "iqn.test:a");
        let b = nexus(2, "iqn.test:b");
        register(&mgr, &a, 0xAAAA);
        register(&mgr, &b, 0xBBBB);
        reserve(&mgr, &a, 0xAAAA, 0x01); // WR_EX (not all-registrants)
        let out = mgr.prout(0, 0x04, 0, 0x01, &params(0xBBBB, 0, false), 24, &b, true);
        assert_eq!(out, PrOutOutcome::InvalidFieldInParameterList);
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
    fn registration_survives_session_loss_no_eviction() {
        // SPC-4 §5.12: a persistent registration is removed only by an
        // explicit PROUT (Release / unregister / Preempt / Clear) — never
        // by I_T nexus loss. There is no longer a drop-on-logout path;
        // a reconnecting initiator keeps its registration.
        let mgr = ReservationManager::new();
        let na = nexus(1, "iqn.test:a");
        register(&mgr, &na, 0xAAAA);
        reserve(&mgr, &na, 0xAAAA, ReservationType::WriteExclusive.as_u8());
        // A non-holder is fenced; the same initiator port (rebuilt as if
        // after a reconnect — identical IQN + ISID, irrespective of any
        // new TSIH) is still recognised as the holder.
        let nb = nexus(2, "iqn.test:b");
        assert!(!mgr.allow_write(0, &nb));
        let a_again = Nexus::iscsi(Some("iqn.test:a".into()), [1u8; 6]);
        assert!(mgr.allow_write(0, &a_again));
    }

    #[test]
    fn mpio_two_isids_one_iqn_are_distinct() {
        // Two sessions from one initiator IQN over different ISIDs are
        // distinct initiator ports => distinct registrants. They
        // round-trip independently.
        let mgr = ReservationManager::new();
        let path_a = Nexus::iscsi(Some("iqn.test:host".into()), [0xA1; 6]);
        let path_b = Nexus::iscsi(Some("iqn.test:host".into()), [0xB2; 6]);
        register(&mgr, &path_a, 0xAAAA);
        register(&mgr, &path_b, 0xBBBB);
        let body = read_keys(&mgr);
        // Two distinct keys listed.
        assert_eq!(body.len(), 24);
        reserve(
            &mgr,
            &path_a,
            0xAAAA,
            ReservationType::ExclusiveAccess.as_u8(),
        );
        // path_b is a different port: under EXCLUSIVE ACCESS it is fenced.
        assert!(!mgr.allow_write(0, &path_b));
        assert!(mgr.allow_write(0, &path_a));
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
    fn prout_aptpl_rejected_when_not_capable() {
        // An in-memory manager (no data dir) advertises PTPL_C=0 and so
        // must reject APTPL=1 rather than silently fail to persist —
        // advertisement and behavior stay consistent.
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
        assert_eq!(
            mgr.register(0, &a, 0, 0xAAAA, true, None),
            PrOutOutcome::Good
        );
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
        mgr.register(0, &a, 0, 0xAAAA, true, None);
        mgr.register(0, &b, 0, 0xBBBB, true, None);
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
    fn iscsi_and_nvme_registrants_coexist_distinctly() {
        // An iSCSI initiator port and an NVMe host are distinct identity
        // namespaces — they never collide as the "same" registrant, and
        // both persist across connection teardown (nothing evicts them).
        let mgr = ReservationManager::new();
        let iscsi = Nexus::iscsi(Some("iqn.test:a".into()), [7u8; 6]);
        let host = nvme_host(0xC3);
        mgr.register(0, &iscsi, 0, 0x1111, true, None);
        mgr.register(0, &host, 0, 0x2222, true, None);
        mgr.reserve(0, &host, 0x2222, ReservationType::ExclusiveAccess.as_u8());

        let snap = mgr.snapshot(0);
        assert_eq!(snap.registrants.len(), 2);
        assert_eq!(snap.holder, Some(host.clone()));
        // The NVMe host holds EXCLUSIVE ACCESS: the iSCSI port (a
        // registrant, but not the holder, of a non-*RO/*AR type) is
        // fenced from both reads and writes.
        assert!(!mgr.allow_write(0, &iscsi));
        assert!(mgr.allow_write(0, &host));
    }

    #[test]
    fn cross_transport_write_exclusive_fences_the_other_transport() {
        // Issue #66 acceptance primitive: one volume exported over both
        // transports keys the same LUN in the shared manager, so a Write
        // Exclusive reservation taken over one transport fences the other
        // transport's writes. An iSCSI initiator port and an NVMe host are
        // distinct registrant identities that never compare equal, so the
        // non-holder is denied regardless of which transport it speaks.

        // Direction 1: iSCSI holds WE -> the NVMe host's writes are fenced.
        let mgr = ReservationManager::new();
        let iscsi = nexus(1, "iqn.test:a");
        let host = nvme_host(0xD4);
        register(&mgr, &iscsi, 0xAAAA);
        reserve(
            &mgr,
            &iscsi,
            0xAAAA,
            ReservationType::WriteExclusive.as_u8(),
        );
        assert!(!mgr.allow_write(0, &host)); // NVMe host (non-holder) fenced
        assert!(mgr.allow_read(0, &host)); // reads open under Write Exclusive
        assert!(mgr.allow_write(0, &iscsi)); // holder may write

        // Direction 2: NVMe holds WE -> the iSCSI initiator's writes are fenced.
        let mgr = ReservationManager::new();
        let iscsi = nexus(1, "iqn.test:a");
        let host = nvme_host(0xD4);
        mgr.register(0, &host, 0, 0xBBBB, true, None);
        assert_eq!(
            mgr.reserve(0, &host, 0xBBBB, ReservationType::WriteExclusive.as_u8()),
            PrOutOutcome::Good
        );
        assert!(!mgr.allow_write(0, &iscsi)); // iSCSI port (non-holder) fenced
        assert!(mgr.allow_read(0, &iscsi));
        assert!(mgr.allow_write(0, &host));
    }

    #[test]
    fn nvme_preempt_across_hosts_replaces_holder() {
        let mgr = ReservationManager::new();
        let a = nvme_host(0xA1);
        let b = nvme_host(0xB2);
        mgr.register(0, &a, 0, 0xAAAA, true, None);
        mgr.register(0, &b, 0, 0xBBBB, true, None);
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
        mgr.register(0, &a, 0, 0xAAAA, true, None);
        mgr.reserve(0, &a, 0xAAAA, ReservationType::ExclusiveAccess.as_u8());
        assert_eq!(
            mgr.release(0, &a, 0xAAAA, ReservationType::ExclusiveAccess.as_u8()),
            PrOutOutcome::Good
        );
        assert!(mgr.snapshot(0).holder.is_none());
        // Registration persists after release (NVMe Release ≠ unregister).
        assert_eq!(mgr.snapshot(0).registrants, vec![(a, 0xAAAA)]);
    }

    // ----------------------------------------------------------------
    // Persistence (PTPL) — issue #57
    // ----------------------------------------------------------------

    use std::sync::atomic::{AtomicU64, Ordering};

    /// Configurable test resolver: a list of (uuid, lun) bindings.
    struct MapResolver(Vec<([u8; 16], u64)>);
    impl EntityResolver for MapResolver {
        fn uuid_for_lun(&self, lun: u64) -> Option<[u8; 16]> {
            self.0.iter().find(|(_, l)| *l == lun).map(|(u, _)| *u)
        }
        fn lun_for_uuid(&self, uuid: &[u8; 16]) -> Option<u64> {
            self.0.iter().find(|(u, _)| u == uuid).map(|(_, l)| *l)
        }
    }

    fn one_volume(uuid: [u8; 16], lun: u64) -> Arc<dyn EntityResolver> {
        Arc::new(MapResolver(vec![(uuid, lun)]))
    }

    /// A unique temp dir for one test (no `tempfile` dep in scsi-spc).
    fn tmp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("thur-resv-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).expect("mkdir");
        d
    }

    // PROUT with an explicit APTPL bit (the `params` helper already
    // takes `aptpl`); register + reserve with APTPL=1.
    fn register_aptpl(mgr: &ReservationManager, n: &Nexus, key: u64) {
        assert_eq!(
            mgr.prout(0, 0x06, 0, 0, &params(0, key, true), 24, n, true),
            PrOutOutcome::Good
        );
    }
    fn reserve_aptpl(mgr: &ReservationManager, n: &Nexus, key: u64, type_byte: u8) {
        assert_eq!(
            mgr.prout(0, 0x01, 0, type_byte, &params(key, 0, true), 24, n, true),
            PrOutOutcome::Good
        );
    }

    #[test]
    fn ptpl_capability_reflects_persistence_wiring() {
        let dir = tmp_dir("cap");
        let path = dir.join("reservations.json");
        let mem = ReservationManager::new();
        assert!(!mem.ptpl_capable());
        let durable = ReservationManager::load_from(path, one_volume([0u8; 16], 0));
        assert!(durable.ptpl_capable());
        // PTPL_C bit (byte 2 bit 0) follows capability.
        let cap = |m: &ReservationManager| match m.prin(0, 0x02, true) {
            PrInOutcome::Good(b) => b[2] & 0x01,
            other => panic!("{other:?}"),
        };
        assert_eq!(cap(&mem), 0x00);
        assert_eq!(cap(&durable), 0x01);
    }

    #[test]
    fn save_then_load_preserves_conflict() {
        let dir = tmp_dir("save-load");
        let path = dir.join("reservations.json");
        let uuid = [0x11u8; 16];
        let a = Nexus::iscsi(Some("iqn.test:a".into()), [1u8; 6]);
        {
            let mgr = ReservationManager::load_from(path.clone(), one_volume(uuid, 0));
            register_aptpl(&mgr, &a, 0xAAAA);
            reserve_aptpl(&mgr, &a, 0xAAAA, ReservationType::WriteExclusive.as_u8());
        }
        // Fresh manager, same file => state survives the "restart".
        let mgr = ReservationManager::load_from(path, one_volume(uuid, 0));
        let b = Nexus::iscsi(Some("iqn.test:b".into()), [2u8; 6]);
        assert!(
            !mgr.allow_write(0, &b),
            "non-holder still fenced after reload"
        );
        assert!(
            mgr.allow_write(0, &a),
            "holder still permitted after reload"
        );
        // READ KEYS still lists A's key; PR_GENERATION preserved (not reset).
        let body = match mgr.prin(0, 0x00, true) {
            PrInOutcome::Good(b) => b,
            other => panic!("{other:?}"),
        };
        assert_eq!(&body[8..16], &0xAAAAu64.to_be_bytes());
        let snap = mgr.snapshot(0);
        assert_eq!(
            snap.generation, 2,
            "generation preserved, not bumped on reload"
        );
    }

    /// Regression for issue #181: a mutation's durable persist runs OFF
    /// the `by_lun` data-path lock, so `allow_read` / `allow_write` on
    /// other LUNs are not gated behind a peer's APTPL-persist fsync.
    /// Exercises the interleaving for deadlock-freedom and correctness
    /// (the old code held `by_lun` across the fsync, so this same shape
    /// would have serialized every read behind the persisting writer).
    #[test]
    fn persist_runs_off_the_data_path_lock() {
        use std::sync::Arc;
        let dir = tmp_dir("concurrent-persist");
        let path = dir.join("reservations.json");
        let resolver: Arc<dyn EntityResolver> =
            Arc::new(MapResolver(vec![([0xA1u8; 16], 0), ([0xB2u8; 16], 1)]));
        let mgr = Arc::new(ReservationManager::load_from(path, resolver));

        // Churn the APTPL persist path on LUN 0 from another thread.
        let writer = {
            let mgr = Arc::clone(&mgr);
            std::thread::spawn(move || {
                let a = Nexus::iscsi(Some("iqn.test:a".into()), [1u8; 6]);
                for _ in 0..100 {
                    // REGISTER K then deregister — each persists (APTPL=1).
                    assert_eq!(
                        mgr.prout(0, 0x06, 0, 0, &params(0, 0xAAAA, true), 24, &a, true),
                        PrOutOutcome::Good
                    );
                    assert_eq!(
                        mgr.prout(0, 0x06, 0, 0, &params(0xAAAA, 0, true), 24, &a, true),
                        PrOutOutcome::Good
                    );
                }
            })
        };

        // LUN 1 has no reservation, so every read must be allowed and must
        // not deadlock or stall waiting on LUN 0's persist fsync.
        let b = Nexus::iscsi(Some("iqn.test:b".into()), [2u8; 6]);
        for _ in 0..200 {
            assert!(mgr.allow_read(1, &b), "unrelated LUN must stay readable");
        }
        writer.join().expect("persisting thread must not deadlock or panic");
    }

    #[test]
    fn reload_rematches_by_port_not_session() {
        // Persist under one "session"; a command from a *different* TSIH
        // (irrelevant now) but the same (IQN, ISID) is the holder after
        // reload. The whole point of keying by initiator port.
        let dir = tmp_dir("rematch");
        let path = dir.join("reservations.json");
        let uuid = [0x22u8; 16];
        let a = Nexus::iscsi(Some("iqn.test:a".into()), [0x5A; 6]);
        {
            let mgr = ReservationManager::load_from(path.clone(), one_volume(uuid, 0));
            register_aptpl(&mgr, &a, 0xAAAA);
            reserve_aptpl(&mgr, &a, 0xAAAA, ReservationType::ExclusiveAccess.as_u8());
        }
        let mgr = ReservationManager::load_from(path, one_volume(uuid, 0));
        let a_reconnected = Nexus::iscsi(Some("iqn.test:a".into()), [0x5A; 6]);
        assert!(mgr.allow_write(0, &a_reconnected));
        // A different ISID under the same IQN is a different port: fenced.
        let other_path = Nexus::iscsi(Some("iqn.test:a".into()), [0x99; 6]);
        assert!(!mgr.allow_write(0, &other_path));
    }

    #[test]
    fn aptpl_false_not_persisted() {
        let dir = tmp_dir("noaptpl");
        let path = dir.join("reservations.json");
        let uuid = [0x33u8; 16];
        let a = Nexus::iscsi(Some("iqn.test:a".into()), [1u8; 6]);
        {
            let mgr = ReservationManager::load_from(path.clone(), one_volume(uuid, 0));
            // APTPL=0 register + reserve: lives in memory, not on disk.
            register(&mgr, &a, 0xAAAA);
            reserve(&mgr, &a, 0xAAAA, ReservationType::WriteExclusive.as_u8());
        }
        // No file written (or empty) => reload sees nothing.
        let mgr = ReservationManager::load_from(path, one_volume(uuid, 0));
        let b = Nexus::iscsi(Some("iqn.test:b".into()), [2u8; 6]);
        assert!(mgr.allow_write(0, &b), "no fence survived APTPL=0 restart");
        assert!(mgr.snapshot(0).registrants.is_empty());
    }

    #[test]
    fn clear_erases_record() {
        let dir = tmp_dir("clear");
        let path = dir.join("reservations.json");
        let uuid = [0x44u8; 16];
        let a = Nexus::iscsi(Some("iqn.test:a".into()), [1u8; 6]);
        {
            let mgr = ReservationManager::load_from(path.clone(), one_volume(uuid, 0));
            register_aptpl(&mgr, &a, 0xAAAA);
            reserve_aptpl(&mgr, &a, 0xAAAA, ReservationType::WriteExclusive.as_u8());
            // CLEAR wipes everything and must erase the on-disk record.
            assert_eq!(mgr.clear(0, &a, 0xAAAA), PrOutOutcome::Good);
        }
        let mgr = ReservationManager::load_from(path, one_volume(uuid, 0));
        let b = Nexus::iscsi(Some("iqn.test:b".into()), [2u8; 6]);
        assert!(mgr.allow_write(0, &b), "cleared fence must not resurrect");
        assert!(mgr.snapshot(0).registrants.is_empty());
    }

    #[test]
    fn reload_torn_file_starts_empty() {
        let dir = tmp_dir("torn");
        let path = dir.join("reservations.json");
        std::fs::write(&path, b"{ this is not json ...").unwrap();
        // Must not panic; starts empty.
        let mgr = ReservationManager::load_from(path, one_volume([0x55u8; 16], 0));
        assert!(mgr.snapshot(0).registrants.is_empty());
        // And it's usable afterward.
        let a = Nexus::iscsi(Some("iqn.test:a".into()), [1u8; 6]);
        register_aptpl(&mgr, &a, 0xAAAA);
        assert_eq!(mgr.snapshot(0).registrants.len(), 1);
    }

    #[test]
    fn lun_reuse_not_fenced() {
        // Persist a fence for volume-A on LUN 3. Volume A is then deleted
        // and a new volume B reuses LUN 3. On reload, A's UUID resolves
        // to no LUN => its record is dropped => B is NOT fenced.
        let dir = tmp_dir("reuse");
        let path = dir.join("reservations.json");
        let uuid_a = [0xAAu8; 16];
        let a = Nexus::iscsi(Some("iqn.test:a".into()), [1u8; 6]);
        {
            let mgr = ReservationManager::load_from(path.clone(), one_volume(uuid_a, 3));
            assert_eq!(
                mgr.prout(3, 0x06, 0, 0, &params(0, 0xAAAA, true), 24, &a, true),
                PrOutOutcome::Good
            );
            assert_eq!(
                mgr.prout(
                    3,
                    0x01,
                    0,
                    ReservationType::WriteExclusive.as_u8(),
                    &params(0xAAAA, 0, true),
                    24,
                    &a,
                    true
                ),
                PrOutOutcome::Good
            );
        }
        // A gone; B (uuid_b) now owns LUN 3. A's UUID maps to no LUN.
        let uuid_b = [0xBBu8; 16];
        let resolver: Arc<dyn EntityResolver> = Arc::new(MapResolver(vec![(uuid_b, 3)]));
        let mgr = ReservationManager::load_from(path, resolver);
        let other = Nexus::iscsi(Some("iqn.test:other".into()), [9u8; 6]);
        assert!(
            mgr.allow_write(3, &other),
            "reused LUN must not inherit gone volume's fence"
        );
        assert!(mgr.snapshot(3).registrants.is_empty());
    }

    #[test]
    fn persist_failure_returns_check_condition_and_rolls_back() {
        // Point persistence at an unwritable path (parent dir absent) so
        // the durable write fails: a persist-eligible mutation must NOT
        // ack Good — it returns PersistFailed (persist-before-ack) — AND
        // it must NOT leave the mutation applied in memory (no ghost
        // fence the host was told failed).
        let dir = tmp_dir("failpersist");
        let bad = dir.join("does-not-exist").join("reservations.json");
        let mgr = ReservationManager::load_from(bad, one_volume([0x66u8; 16], 0));
        let a = Nexus::iscsi(Some("iqn.test:a".into()), [1u8; 6]);
        // APTPL=1 register => persist-eligible => write fails => PersistFailed.
        let out = mgr.prout(0, 0x06, 0, 0, &params(0, 0xAAAA, true), 24, &a, true);
        assert_eq!(out, PrOutOutcome::PersistFailed);
        // Rolled back: no registration lingers, so a peer is not fenced.
        assert!(
            mgr.snapshot(0).registrants.is_empty(),
            "failed persist must not leave a ghost registration"
        );
        let b = Nexus::iscsi(Some("iqn.test:b".into()), [2u8; 6]);
        assert!(mgr.allow_write(0, &b), "no ghost fence after PersistFailed");
    }

    #[test]
    fn purge_lun_drops_state_in_memory_and_on_disk() {
        let dir = tmp_dir("purge");
        let path = dir.join("reservations.json");
        let uuid = [0x77u8; 16];
        let a = Nexus::iscsi(Some("iqn.test:a".into()), [1u8; 6]);
        let mgr = ReservationManager::load_from(path.clone(), one_volume(uuid, 0));
        register_aptpl(&mgr, &a, 0xAAAA);
        reserve_aptpl(&mgr, &a, 0xAAAA, ReservationType::WriteExclusive.as_u8());
        mgr.purge_lun(0);
        // In-memory state gone (so a reused LUN starts clean), and a
        // fresh reload sees nothing either.
        assert!(mgr.snapshot(0).registrants.is_empty());
        let reloaded = ReservationManager::load_from(path, one_volume(uuid, 0));
        assert!(reloaded.snapshot(0).registrants.is_empty());
    }

    // ----------------------------------------------------------------
    // Proactive cross-transport notification hook (issue #67)
    // ----------------------------------------------------------------

    #[derive(Default)]
    struct CaptureObserver {
        seen: Mutex<Vec<ReservationChange>>,
    }
    impl ReservationObserver for CaptureObserver {
        fn on_reservation_change(&self, changes: &[ReservationChange]) {
            self.seen
                .lock()
                .expect("capture mutex")
                .extend_from_slice(changes);
        }
    }
    impl CaptureObserver {
        fn take(&self) -> Vec<ReservationChange> {
            std::mem::take(&mut self.seen.lock().expect("capture mutex"))
        }
    }

    // REGISTER (sark != 0) + RESERVE (Acquire) fence nobody, so the hook
    // fires nothing even though both mutate state.
    #[test]
    fn register_and_acquire_emit_no_changes() {
        let mgr = ReservationManager::new();
        let obs = Arc::new(CaptureObserver::default());
        mgr.register_observer(obs.clone());
        let a = nexus(1, "iqn.test:a");
        register(&mgr, &a, 0xAAAA);
        reserve(&mgr, &a, 0xAAAA, ReservationType::WriteExclusive.as_u8());
        assert!(obs.take().is_empty());
    }

    // The headline #67 gap: a preempt issued over one transport surfaces
    // the fenced registrant on the OTHER transport. NVMe host B preempts
    // an iSCSI holder A → A gets ReservationPreempted.
    #[test]
    fn cross_transport_preempt_notifies_iscsi_holder() {
        let mgr = ReservationManager::new();
        let obs = Arc::new(CaptureObserver::default());
        mgr.register_observer(obs.clone());
        let iscsi_a = nexus(1, "iqn.test:a");
        let nvme_b = nvme_host(0xBB);
        mgr.register(0, &iscsi_a, 0, 0xAAAA, true, Some(false));
        mgr.reserve(
            0,
            &iscsi_a,
            0xAAAA,
            ReservationType::ExclusiveAccess.as_u8(),
        );
        mgr.register(0, &nvme_b, 0, 0xBBBB, true, Some(false));
        let _ = obs.take(); // discard the (empty) register/acquire window
        assert_eq!(
            mgr.preempt(
                0,
                &nvme_b,
                0xBBBB,
                0xAAAA,
                ReservationType::ExclusiveAccess.as_u8()
            ),
            PrOutOutcome::Good
        );
        let changes = obs.take();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].affected, iscsi_a);
        assert_eq!(changes[0].kind, ReservationChangeKind::ReservationPreempted);
        assert_eq!(changes[0].lun, 0);
    }

    // The pre-existing iSCSI->iSCSI gap: a RELEASE now fans
    // ReservationReleased to surviving iSCSI registrants (was silent).
    #[test]
    fn iscsi_release_notifies_surviving_iscsi_registrants() {
        let mgr = ReservationManager::new();
        let obs = Arc::new(CaptureObserver::default());
        mgr.register_observer(obs.clone());
        let a = nexus(1, "iqn.test:a");
        let b = nexus(2, "iqn.test:b");
        mgr.register(0, &a, 0, 0xAAAA, true, Some(false));
        mgr.register(0, &b, 0, 0xBBBB, true, Some(false));
        mgr.reserve(0, &a, 0xAAAA, ReservationType::WriteExclusive.as_u8());
        let _ = obs.take();
        assert_eq!(
            mgr.release(0, &a, 0xAAAA, ReservationType::WriteExclusive.as_u8()),
            PrOutOutcome::Good
        );
        let changes = obs.take();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].affected, b);
        assert_eq!(changes[0].kind, ReservationChangeKind::ReservationReleased);
    }

    // CLEAR surfaces BOTH transports' registrants in one slice, issuer
    // excluded.
    #[test]
    fn clear_notifies_both_transports() {
        let mgr = ReservationManager::new();
        let obs = Arc::new(CaptureObserver::default());
        mgr.register_observer(obs.clone());
        let iscsi_a = nexus(1, "iqn.test:a");
        let nvme_b = nvme_host(0xBB);
        let nvme_c = nvme_host(0xCC);
        mgr.register(0, &iscsi_a, 0, 0xAAAA, true, Some(false));
        mgr.register(0, &nvme_b, 0, 0xBBBB, true, Some(false));
        mgr.register(0, &nvme_c, 0, 0xCCCC, true, Some(false));
        mgr.reserve(0, &nvme_c, 0xCCCC, ReservationType::WriteExclusive.as_u8());
        let _ = obs.take();
        assert_eq!(mgr.clear(0, &nvme_c, 0xCCCC), PrOutOutcome::Good);
        let mut got: Vec<RegistrantId> = obs
            .take()
            .into_iter()
            .map(|c| {
                assert_eq!(c.kind, ReservationChangeKind::ReservationPreempted);
                c.affected
            })
            .collect();
        got.sort();
        let mut want = vec![iscsi_a, nvme_b]; // issuer nvme_c excluded
        want.sort();
        assert_eq!(got, want);
    }

    // Issuer exclusion is uniform across transports: an iSCSI issuer that
    // clears is itself never notified, but its NVMe peer is.
    #[test]
    fn issuer_excluded_for_both_transports() {
        let mgr = ReservationManager::new();
        let obs = Arc::new(CaptureObserver::default());
        mgr.register_observer(obs.clone());
        let iscsi_a = nexus(1, "iqn.test:a");
        let nvme_b = nvme_host(0xBB);
        mgr.register(0, &iscsi_a, 0, 0xAAAA, true, Some(false));
        mgr.register(0, &nvme_b, 0, 0xBBBB, true, Some(false));
        let _ = obs.take();
        mgr.clear(0, &iscsi_a, 0xAAAA);
        let changes = obs.take();
        assert!(changes.iter().all(|c| c.affected != iscsi_a));
        assert!(changes.iter().any(|c| c.affected == nvme_b));
    }

    // A conflicting op (preempt by an unregistered initiator) mutates
    // nothing and must not notify.
    #[test]
    fn conflict_emits_no_changes() {
        let mgr = ReservationManager::new();
        let obs = Arc::new(CaptureObserver::default());
        mgr.register_observer(obs.clone());
        let a = nexus(1, "iqn.test:a");
        let b = nexus(2, "iqn.test:b");
        mgr.register(0, &a, 0, 0xAAAA, true, Some(false));
        mgr.reserve(0, &a, 0xAAAA, ReservationType::ExclusiveAccess.as_u8());
        let _ = obs.take();
        assert_eq!(
            mgr.preempt(
                0,
                &b,
                0xBBBB,
                0xAAAA,
                ReservationType::ExclusiveAccess.as_u8()
            ),
            PrOutOutcome::ReservationConflict
        );
        assert!(obs.take().is_empty());
    }

    // persist-before-ack: a PersistFailed mutation rolls back AND must
    // not notify peers of a fence the host was told failed.
    #[test]
    fn persist_failure_emits_no_changes_and_rolls_back() {
        let dir = tmp_dir("notify-failpersist");
        let path = dir.join("reservations.json");
        let obs = Arc::new(CaptureObserver::default());
        let mgr = ReservationManager::load_from(path, one_volume([0x99u8; 16], 0));
        mgr.register_observer(obs.clone());
        let a = nexus(1, "iqn.test:a");
        let b = nexus(2, "iqn.test:b");
        register_aptpl(&mgr, &a, 0xAAAA);
        reserve_aptpl(&mgr, &a, 0xAAAA, ReservationType::ExclusiveAccess.as_u8());
        register_aptpl(&mgr, &b, 0xBBBB);
        let _ = obs.take();
        // Make the next durable write fail (parent dir vanishes).
        std::fs::remove_dir_all(&dir).expect("rm dir");
        assert_eq!(
            mgr.preempt(
                0,
                &b,
                0xBBBB,
                0xAAAA,
                ReservationType::ExclusiveAccess.as_u8()
            ),
            PrOutOutcome::PersistFailed
        );
        assert!(obs.take().is_empty(), "no notification on PersistFailed");
        // Rolled back: A still holds, B still fenced.
        assert!(mgr.allow_write(0, &a));
        assert!(!mgr.allow_write(0, &b));
    }

    // Volume-destroy (purge_lun) bypasses `mutate`; it must stay silent.
    #[test]
    fn purge_lun_emits_no_changes() {
        let mgr = ReservationManager::new();
        let obs = Arc::new(CaptureObserver::default());
        mgr.register_observer(obs.clone());
        let a = nexus(1, "iqn.test:a");
        let b = nexus(2, "iqn.test:b");
        mgr.register(0, &a, 0, 0xAAAA, true, Some(false));
        mgr.register(0, &b, 0, 0xBBBB, true, Some(false));
        mgr.reserve(0, &a, 0xAAAA, ReservationType::WriteExclusive.as_u8());
        let _ = obs.take();
        mgr.purge_lun(0);
        assert!(obs.take().is_empty(), "volume-destroy must not notify");
    }

    // Every registered observer sees the same change slice.
    #[test]
    fn all_observers_receive_changes() {
        let mgr = ReservationManager::new();
        let o1 = Arc::new(CaptureObserver::default());
        let o2 = Arc::new(CaptureObserver::default());
        mgr.register_observer(o1.clone());
        mgr.register_observer(o2.clone());
        let a = nexus(1, "iqn.test:a");
        let b = nexus(2, "iqn.test:b");
        mgr.register(0, &a, 0, 0xAAAA, true, Some(false));
        mgr.register(0, &b, 0, 0xBBBB, true, Some(false));
        mgr.reserve(0, &a, 0xAAAA, ReservationType::WriteExclusive.as_u8());
        let _ = (o1.take(), o2.take());
        mgr.release(0, &a, 0xAAAA, ReservationType::WriteExclusive.as_u8());
        assert_eq!(o1.take(), o2.take());
    }

    // ---- pure diff_reservation_changes table tests (migrated from
    // nvme/nvm/src/aer.rs, generalized over transport) ----------------

    const HOST_A: [u8; 16] = [0xAA; 16];
    const HOST_B: [u8; 16] = [0xBB; 16];
    const HOST_C: [u8; 16] = [0xCC; 16];

    // NVMe-keyed snapshot (mirrors the old aer.rs helper).
    fn dsnap(holder: Option<[u8; 16]>, regs: &[[u8; 16]]) -> ReservationSnapshot {
        ReservationSnapshot {
            generation: 0,
            reservation_type: None,
            holder: holder.map(RegistrantId::nvme),
            registrants: regs.iter().map(|h| (RegistrantId::nvme(*h), 1)).collect(),
            aptpl: false,
        }
    }
    fn chg(affected: RegistrantId, kind: ReservationChangeKind) -> ReservationChange {
        ReservationChange {
            lun: 0,
            affected,
            kind,
        }
    }

    #[test]
    fn diff_preempt_non_holder_is_registration_preempted() {
        let pre = dsnap(None, &[HOST_A, HOST_B]);
        let post = dsnap(None, &[HOST_B]);
        assert_eq!(
            diff_reservation_changes(
                ResvAction::Preempt,
                0,
                &RegistrantId::nvme(HOST_B),
                &pre,
                &post
            ),
            vec![chg(
                RegistrantId::nvme(HOST_A),
                ReservationChangeKind::RegistrationPreempted
            )]
        );
    }

    #[test]
    fn diff_preempt_holder_is_reservation_preempted_only() {
        let pre = dsnap(Some(HOST_A), &[HOST_A, HOST_B]);
        let post = dsnap(Some(HOST_B), &[HOST_B]);
        assert_eq!(
            diff_reservation_changes(
                ResvAction::Preempt,
                0,
                &RegistrantId::nvme(HOST_B),
                &pre,
                &post
            ),
            vec![chg(
                RegistrantId::nvme(HOST_A),
                ReservationChangeKind::ReservationPreempted
            )]
        );
    }

    #[test]
    fn diff_preempt_holder_plus_other_registrant() {
        let pre = dsnap(Some(HOST_A), &[HOST_A, HOST_B, HOST_C]);
        let post = dsnap(Some(HOST_B), &[HOST_B]);
        let changes = diff_reservation_changes(
            ResvAction::Preempt,
            0,
            &RegistrantId::nvme(HOST_B),
            &pre,
            &post,
        );
        assert!(changes.contains(&chg(
            RegistrantId::nvme(HOST_A),
            ReservationChangeKind::ReservationPreempted
        )));
        assert!(changes.contains(&chg(
            RegistrantId::nvme(HOST_C),
            ReservationChangeKind::RegistrationPreempted
        )));
        assert_eq!(changes.len(), 2);
    }

    #[test]
    fn diff_release_fans_out_to_others() {
        let pre = dsnap(Some(HOST_A), &[HOST_A, HOST_B, HOST_C]);
        let post = dsnap(None, &[HOST_A, HOST_B, HOST_C]);
        let changes = diff_reservation_changes(
            ResvAction::Release,
            0,
            &RegistrantId::nvme(HOST_A),
            &pre,
            &post,
        );
        assert!(changes.contains(&chg(
            RegistrantId::nvme(HOST_B),
            ReservationChangeKind::ReservationReleased
        )));
        assert!(changes.contains(&chg(
            RegistrantId::nvme(HOST_C),
            ReservationChangeKind::ReservationReleased
        )));
        assert!(
            !changes
                .iter()
                .any(|c| c.affected == RegistrantId::nvme(HOST_A))
        );
        assert_eq!(changes.len(), 2);
    }

    #[test]
    fn diff_self_unregister_holder_releases_to_survivors() {
        let pre = dsnap(Some(HOST_A), &[HOST_A, HOST_B]);
        let post = dsnap(None, &[HOST_B]);
        assert_eq!(
            diff_reservation_changes(
                ResvAction::Unregister,
                0,
                &RegistrantId::nvme(HOST_A),
                &pre,
                &post
            ),
            vec![chg(
                RegistrantId::nvme(HOST_B),
                ReservationChangeKind::ReservationReleased
            )]
        );
    }

    #[test]
    fn diff_clear_preempts_all_other_registrants() {
        let pre = dsnap(Some(HOST_A), &[HOST_A, HOST_B, HOST_C]);
        let post = dsnap(None, &[]);
        let changes = diff_reservation_changes(
            ResvAction::Clear,
            0,
            &RegistrantId::nvme(HOST_A),
            &pre,
            &post,
        );
        assert!(changes.contains(&chg(
            RegistrantId::nvme(HOST_B),
            ReservationChangeKind::ReservationPreempted
        )));
        assert!(changes.contains(&chg(
            RegistrantId::nvme(HOST_C),
            ReservationChangeKind::ReservationPreempted
        )));
        assert_eq!(changes.len(), 2);
    }

    #[test]
    fn diff_all_registrants_holder_rotation_emits_nothing() {
        // Holder A unregisters; the all-registrants reservation rotates to
        // B (post.holder still set) → nothing fans out.
        let pre = dsnap(Some(HOST_A), &[HOST_A, HOST_B]);
        let post = dsnap(Some(HOST_B), &[HOST_B]);
        assert!(
            diff_reservation_changes(
                ResvAction::Unregister,
                0,
                &RegistrantId::nvme(HOST_A),
                &pre,
                &post
            )
            .is_empty()
        );
    }

    #[test]
    fn diff_idempotent_reacquire_emits_nothing() {
        let pre = dsnap(Some(HOST_A), &[HOST_A, HOST_B]);
        let post = dsnap(Some(HOST_A), &[HOST_A, HOST_B]);
        assert!(
            diff_reservation_changes(
                ResvAction::Preempt,
                0,
                &RegistrantId::nvme(HOST_A),
                &pre,
                &post
            )
            .is_empty()
        );
    }

    #[test]
    fn diff_mixed_transport_clear_surfaces_both() {
        // An NVMe holder clears; an iSCSI peer and an NVMe peer both get
        // ReservationPreempted — proving the diff is transport-neutral.
        let iscsi_x = RegistrantId::iscsi(Some("iqn.test:x".into()), [7u8; 6]);
        let pre = ReservationSnapshot {
            generation: 0,
            reservation_type: None,
            holder: Some(RegistrantId::nvme(HOST_A)),
            registrants: vec![
                (RegistrantId::nvme(HOST_A), 1),
                (RegistrantId::nvme(HOST_B), 1),
                (iscsi_x.clone(), 1),
            ],
            aptpl: false,
        };
        let post = dsnap(None, &[]);
        let changes = diff_reservation_changes(
            ResvAction::Clear,
            0,
            &RegistrantId::nvme(HOST_A),
            &pre,
            &post,
        );
        assert!(changes.contains(&chg(
            RegistrantId::nvme(HOST_B),
            ReservationChangeKind::ReservationPreempted
        )));
        assert!(changes.contains(&chg(iscsi_x, ReservationChangeKind::ReservationPreempted)));
        assert_eq!(changes.len(), 2);
    }
}
