// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-subsystem controller registry + Asynchronous Event Request hub
//! + reservation-notification event derivation.
//!
//! [`ControllerRegistry`] is the subsystem-wide controller table shared
//! (one `Arc`) between the NVM dispatcher and the NVMe/TCP transport.
//! It does three jobs:
//!
//! 1. **CNTLID allocation + controller lifecycle.** An admin-queue
//!    Connect ([`ControllerRegistry::connect_admin`]) creates a
//!    controller and is assigned a fresh CNTLID; an I/O-queue Connect
//!    ([`ControllerRegistry::connect_io`]) attaches to the controller
//!    the host names in Connect Data CNTLID. A controller — and its
//!    CNTLID — is freed when its last association drops
//!    ([`ControllerRegistry::disconnect`]). Reservation registrations
//!    are NOT freed with it: they persist by HOSTID in the reservation
//!    manager (see #54).
//! 2. **AER delivery.** Each connection's reader parks an AER via
//!    [`ControllerRegistry::park`] and a delivery task awaits the
//!    oneshot. Two event sources complete parked AERs:
//!    - a reservation op on an I/O queue calls
//!      [`ControllerRegistry::notify`], which fans the event to every
//!      live controller of the *affected host* and completes their
//!      parked AERs (LID 0x80);
//!    - a volume create / destroy on the admin socket calls
//!      [`ControllerRegistry::notify_namespace_attribute_changed`],
//!      which fans to *every* live controller that has enabled the
//!      notice (FID 0x0B bit 8) and completes their parked AERs
//!      (LID 0x04).
//!
//!    Either way the completion is cross-connection: a command on one
//!    TCP connection completes an AER parked on a controller's admin
//!    queue, possibly on a different connection.
//! 3. **Per-controller log + feature state.** The LID 0x80 reservation
//!    notification log ([`ControllerRegistry::take_log_entry`]), the
//!    FID 0x82 Reservation Notification Mask
//!    ([`ControllerRegistry::set_mask`] / [`ControllerRegistry::get_mask`]),
//!    the LID 0x04 Changed Namespace List
//!    ([`ControllerRegistry::take_changed_namespaces`]), and the FID
//!    0x0B Async Event Configuration
//!    ([`ControllerRegistry::set_aen_config`] /
//!    [`ControllerRegistry::get_aen_config`]) are all per-controller
//!    (keyed by CNTLID) — each controller has its own log pages and
//!    feature settings, and all vanish when the controller is freed.
//!
//! Notifications target the affected host's controllers; the issuing
//! host is excluded at derivation time (`diff_reservation_events`), so
//! a host never receives an asynchronous notice for its own command.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nvme_base::aer::{
    AEI_NAMESPACE_ATTRIBUTE_CHANGED, AEI_RESERVATION_LOG_AVAILABLE, AEN_CFG_NAMESPACE_ATTRIBUTE,
    AET_IO_COMMAND_SET, AET_NOTICE, async_event_dw0,
};
use nvme_base::log_page::{self, lid, resv_notif_type};
use scsi_spc::reservations::{
    RegistrantId, ReservationChange, ReservationChangeKind, ReservationObserver,
};
use tokio::sync::oneshot;

/// Upper bound on queued-but-unread notifications per host. A host
/// that never reads LID 0x80 can't grow the queue without limit; the
/// oldest entries are dropped first (the host learns the count is
/// capped via the "number of available log pages" byte).
const MAX_QUEUED_NOTIFICATIONS: usize = 64;

/// DW0 a parked AER completes with: AET=I/O Command Set specific,
/// AEI=Reservation Log Page Available, LID=0x80. Const-folded.
const RESERVATION_NOTICE_DW0: u32 = async_event_dw0(
    AET_IO_COMMAND_SET,
    AEI_RESERVATION_LOG_AVAILABLE,
    lid::RESERVATION_NOTIFICATION,
);

/// DW0 a parked AER completes with for a namespace-attribute change:
/// AET=Notice, AEI=Namespace Attribute Changed, LID=0x04 (Changed
/// Namespace List). Const-folded.
const NAMESPACE_NOTICE_DW0: u32 = async_event_dw0(
    AET_NOTICE,
    AEI_NAMESPACE_ATTRIBUTE_CHANGED,
    lid::CHANGED_NAMESPACE_LIST,
);

/// Which reservation event a notification reports. Maps onto the LID
/// 0x80 type byte and the FID 0x82 mask bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationEventKind {
    RegistrationPreempted,
    ReservationReleased,
    ReservationPreempted,
}

impl ReservationEventKind {
    fn type_byte(self) -> u8 {
        match self {
            Self::RegistrationPreempted => resv_notif_type::REGISTRATION_PREEMPTED,
            Self::ReservationReleased => resv_notif_type::RESERVATION_RELEASED,
            Self::ReservationPreempted => resv_notif_type::RESERVATION_PREEMPTED,
        }
    }

    /// FID 0x82 Reservation Notification Mask bit that suppresses this
    /// kind: bit 1 RegPreempted, bit 2 ResvReleased, bit 3 ResvPreempted.
    fn mask_bit(self) -> u32 {
        match self {
            Self::RegistrationPreempted => 1 << 1,
            Self::ReservationReleased => 1 << 2,
            Self::ReservationPreempted => 1 << 3,
        }
    }
}

/// A reservation event destined for one host on one namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReservationEvent {
    pub host_id: [u8; 16],
    pub nsid: u32,
    pub kind: ReservationEventKind,
}

/// Completion payload handed to a parked AER when an event fires.
#[derive(Debug, Clone, Copy)]
pub struct AerCompletion {
    pub dw0: u32,
}

/// Highest CNTLID the allocator hands out. NVMe reserves 0xFFFF
/// (`CNTLID_ANY`) and 0xFFFE (`CNTLID_STATIC_FLAG`) as special values
/// in the Connect path, and 0 is the "no live controller" sentinel the
/// Reservation Report uses, so the assignable range is 1..=0xFFFD.
const MAX_CNTLID: u16 = 0xFFFD;

/// Why a Fabrics Connect failed to bind to a controller. The transport
/// maps every variant onto `Connect Invalid Parameters`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectError {
    /// All assignable CNTLIDs are in use — not reachable in practice
    /// (a host runs out of sockets long before 65533 controllers).
    NoCntlidAvailable,
    /// An I/O-queue Connect named a CNTLID with no live controller.
    UnknownController(u16),
    /// An I/O-queue Connect named a controller owned by a different
    /// HOSTID — a host may only attach I/O queues to its own controller.
    HostMismatch(u16),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCntlidAvailable => write!(f, "no CNTLID available"),
            Self::UnknownController(c) => {
                write!(f, "I/O Connect to unknown controller CNTLID {c}")
            }
            Self::HostMismatch(c) => write!(f, "I/O Connect to CNTLID {c} owned by another host"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Opaque per-connection handle minted by
/// [`ControllerRegistry::connect_admin`] /
/// [`ControllerRegistry::connect_io`] and surrendered at teardown via
/// [`ControllerRegistry::disconnect`]. Names the controller association
/// the connection drives.
#[derive(Debug, Clone, Copy)]
pub struct ConnToken {
    /// Unique per connection — the key parked AERs are filed under, so
    /// teardown drops only this connection's AERs.
    id: u64,
    /// The controller (CNTLID) this connection is bound to.
    cntlid: u16,
}

impl ConnToken {
    /// The CNTLID assigned to (admin Connect) or attached by (I/O
    /// Connect) this connection. The transport echoes it in the Connect
    /// Response DW0 and threads it into Identify Controller + the
    /// per-controller admin state.
    pub fn cntlid(&self) -> u16 {
        self.cntlid
    }
}

struct ParkedAer {
    conn_id: u64,
    tx: oneshot::Sender<AerCompletion>,
}

/// Per-controller runtime state, keyed by CNTLID in [`Inner`].
struct ControllerState {
    /// HOSTID that created this controller (its admin Connect). The
    /// reservation registrant key and the fan-out target for `notify`.
    host_id: [u8; 16],
    /// Live connections bound to this controller (one admin queue + N
    /// I/O queues). The controller is freed when this reaches 0.
    associations: u32,
    /// Outstanding AERs on this controller's admin queue.
    parked: Vec<ParkedAer>,
    /// Unread reservation notifications: `(log_page_count, type, nsid)`,
    /// oldest at the front.
    pending: VecDeque<(u64, u8, u32)>,
    /// Per-namespace FID 0x82 Reservation Notification Mask.
    masks: HashMap<u32, u32>,
    /// NSIDs whose attributes changed since this controller last read
    /// the Changed Namespace List (LID 0x04). A set so repeated changes
    /// to one namespace collapse; drained on Get Log Page LID 0x04.
    changed_ns: BTreeSet<u32>,
    /// Controller-wide FID 0x0B Async Event Configuration. Bit 8
    /// (`AEN_CFG_NAMESPACE_ATTRIBUTE`) gates namespace-change notices;
    /// defaults to 0 (host hasn't opted in yet).
    aen_config: u32,
    /// FID 0x07 Number of Queues grant `(nsq, ncq)`, both zero-based.
    /// Per-controller — one host's Set Features must not clobber
    /// another's negotiated count (issue #245). `None` until the host
    /// negotiates; the dispatcher then reports its cap.
    num_io_queues: Option<(u16, u16)>,
    /// FID 0x0F Keep Alive Timeout in ms, per-controller (issue #245).
    /// Defaults to 0 (host hasn't set it).
    kato_ms: u32,
}

struct Inner {
    /// Live controllers keyed by CNTLID. A missing key means the
    /// controller has been freed (all its associations dropped).
    controllers: HashMap<u16, ControllerState>,
}

/// Per-subsystem controller registry + AER hub. Cheap to share via
/// `Arc`; all methods take `&self`.
pub struct ControllerRegistry {
    inner: Mutex<Inner>,
    next_conn_id: AtomicU64,
    /// Subsystem-global, monotonically increasing Log Page Count.
    /// Starts at 1 so 0 stays the "empty page" sentinel. A global
    /// counter is still monotonic per controller, which is all the
    /// host needs to detect a fresh notification.
    next_log_count: AtomicU64,
}

impl ControllerRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                controllers: HashMap::new(),
            }),
            next_conn_id: AtomicU64::new(1),
            next_log_count: AtomicU64::new(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Admin-queue Connect (QID 0): allocate a fresh CNTLID and create
    /// the controller for `host_id`. The returned token's `cntlid()` is
    /// echoed to the host in the Connect Response DW0.
    pub fn connect_admin(&self, host_id: [u8; 16]) -> Result<ConnToken, ConnectError> {
        let mut inner = self.lock();
        // Lowest-free allocation: distinct among live controllers, and
        // a freed CNTLID is reused by the next admin Connect.
        let cntlid = (1..=MAX_CNTLID)
            .find(|c| !inner.controllers.contains_key(c))
            .ok_or(ConnectError::NoCntlidAvailable)?;
        inner.controllers.insert(
            cntlid,
            ControllerState {
                host_id,
                associations: 1,
                parked: Vec::new(),
                pending: VecDeque::new(),
                masks: HashMap::new(),
                changed_ns: BTreeSet::new(),
                aen_config: 0,
                num_io_queues: None,
                kato_ms: 0,
            },
        );
        let id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        Ok(ConnToken { id, cntlid })
    }

    /// I/O-queue Connect (QID > 0): attach to the controller the host
    /// names in `requested_cntlid` — the CNTLID it received from its
    /// admin Connect. Validates that the controller exists and belongs
    /// to this HOSTID.
    pub fn connect_io(
        &self,
        host_id: [u8; 16],
        requested_cntlid: u16,
    ) -> Result<ConnToken, ConnectError> {
        let mut inner = self.lock();
        let state = inner
            .controllers
            .get_mut(&requested_cntlid)
            .ok_or(ConnectError::UnknownController(requested_cntlid))?;
        if state.host_id != host_id {
            return Err(ConnectError::HostMismatch(requested_cntlid));
        }
        state.associations += 1;
        let id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        Ok(ConnToken {
            id,
            cntlid: requested_cntlid,
        })
    }

    /// Surrender a connection at teardown. Drops the connection's parked
    /// AERs (their oneshot senders close, unblocking the transport's
    /// delivery tasks) and decrements the controller's association
    /// count. When the last association drops, the controller — its
    /// CNTLID, pending notification log, and FID 0x82 masks — is freed;
    /// returns `true` in that case.
    ///
    /// Reservation registrations are deliberately NOT touched: they
    /// persist by HOSTID in the reservation manager (see #54), so a
    /// host's registration survives the CNTLID being freed.
    pub fn disconnect(&self, token: ConnToken) -> bool {
        let mut inner = self.lock();
        let Some(state) = inner.controllers.get_mut(&token.cntlid) else {
            return false;
        };
        state.parked.retain(|p| p.conn_id != token.id);
        state.associations = state.associations.saturating_sub(1);
        if state.associations == 0 {
            inner.controllers.remove(&token.cntlid);
            true
        } else {
            false
        }
    }

    /// Number of live NVMe controllers (one per host admin Connect that
    /// still has at least one association). Used as the NVMe/TCP
    /// "sessions" count on the monitor / Web UI surface.
    pub fn controller_count(&self) -> usize {
        self.lock().controllers.len()
    }

    /// Park an AER from a connection's admin queue. If the controller
    /// already has an unread notification, complete the AER immediately
    /// (announce availability) instead of parking it. Reservation
    /// notifications take precedence over namespace-change notices when
    /// both are pending; the host re-issues an AER after draining each
    /// log, so the other is announced on the next park.
    pub fn park(&self, token: ConnToken, tx: oneshot::Sender<AerCompletion>) {
        let mut inner = self.lock();
        let Some(state) = inner.controllers.get_mut(&token.cntlid) else {
            // Controller already torn down — let `tx` drop so the
            // delivery task observes a closed channel and exits.
            return;
        };
        if !state.pending.is_empty() {
            let _ = tx.send(AerCompletion {
                dw0: RESERVATION_NOTICE_DW0,
            });
            return;
        }
        if !state.changed_ns.is_empty() {
            let _ = tx.send(AerCompletion {
                dw0: NAMESPACE_NOTICE_DW0,
            });
            return;
        }
        state.parked.push(ParkedAer {
            conn_id: token.id,
            tx,
        });
    }

    /// Record a reservation event and fan it out to every live
    /// controller of the affected host: per-controller mask-check,
    /// append to that controller's log queue, and complete one of its
    /// parked AERs (if any) to announce it. A host with no live
    /// controller drops the event — it learns the new state via a
    /// Reservation Report when it reconnects.
    pub fn notify(&self, event: ReservationEvent) {
        let mut inner = self.lock();
        let targets: Vec<u16> = inner
            .controllers
            .iter()
            .filter(|(_, s)| s.host_id == event.host_id)
            .map(|(&c, _)| c)
            .collect();
        for cntlid in targets {
            let state = inner
                .controllers
                .get_mut(&cntlid)
                .expect("cntlid was just listed under the same lock");
            let mask = state.masks.get(&event.nsid).copied().unwrap_or(0);
            if mask & event.kind.mask_bit() != 0 {
                continue;
            }
            let count = self.next_log_count.fetch_add(1, Ordering::Relaxed);
            if state.pending.len() >= MAX_QUEUED_NOTIFICATIONS {
                state.pending.pop_front();
            }
            state
                .pending
                .push_back((count, event.kind.type_byte(), event.nsid));
            if let Some(parked) = state.parked.pop() {
                let _ = parked.tx.send(AerCompletion {
                    dw0: RESERVATION_NOTICE_DW0,
                });
            }
        }
    }

    /// Build the LID 0x80 page for a controller's Get Log Page: read its
    /// oldest unread notification, or the all-zero empty page when the
    /// queue is drained / the controller is unknown. `consume` removes
    /// the entry (decrementing the available count); `false` peeks
    /// without clearing — used when the host set RAE or supplied a buffer
    /// too short to hold the full page, so the notification isn't lost
    /// (issue #247). The reported `num_available` (entries beyond the
    /// head) is identical either way.
    pub fn take_log_entry(
        &self,
        cntlid: u16,
        consume: bool,
    ) -> [u8; log_page::RESERVATION_NOTIFICATION_LEN] {
        let mut inner = self.lock();
        if let Some(state) = inner.controllers.get_mut(&cntlid) {
            if consume {
                if let Some((count, type_byte, nsid)) = state.pending.pop_front() {
                    let num_available = state.pending.len().min(u8::MAX as usize) as u8;
                    return log_page::reservation_notification(count, type_byte, num_available, nsid);
                }
            } else if let Some(&(count, type_byte, nsid)) = state.pending.front() {
                let num_available = (state.pending.len() - 1).min(u8::MAX as usize) as u8;
                return log_page::reservation_notification(count, type_byte, num_available, nsid);
            }
        }
        log_page::reservation_notification(0, resv_notif_type::EMPTY, 0, 0)
    }

    /// Store the FID 0x82 Reservation Notification Mask for
    /// `(cntlid, nsid)`. No-op if the controller is unknown.
    pub fn set_mask(&self, cntlid: u16, nsid: u32, mask: u32) {
        let mut inner = self.lock();
        if let Some(state) = inner.controllers.get_mut(&cntlid) {
            state.masks.insert(nsid, mask);
        }
    }

    /// Read the FID 0x82 mask for `(cntlid, nsid)` (0 = all enabled).
    pub fn get_mask(&self, cntlid: u16, nsid: u32) -> u32 {
        let inner = self.lock();
        inner
            .controllers
            .get(&cntlid)
            .and_then(|s| s.masks.get(&nsid).copied())
            .unwrap_or(0)
    }

    /// Record a namespace-attribute change (a volume create / destroy /
    /// resize) and fan it out to *every* live controller that has
    /// enabled namespace-change notices (FID 0x0B bit 8): append `nsid`
    /// to that controller's Changed Namespace List and complete one of
    /// its parked AERs (if any) to announce it.
    ///
    /// Unlike [`notify`](Self::notify), this is a subsystem-wide event
    /// — not host-scoped — so it reaches all hosts, mirroring how a
    /// physical multi-controller subsystem reports namespace changes to
    /// every attached controller. A controller that hasn't opted in is
    /// skipped entirely (no list entry, no AER), so a host that never
    /// set FID 0x0B bit 8 is undisturbed. With no live controllers (NVMe
    /// transport disabled, or no host connected) this is a no-op.
    pub fn notify_namespace_attribute_changed(&self, nsid: u32) {
        let mut inner = self.lock();
        let targets: Vec<u16> = inner
            .controllers
            .iter()
            .filter(|(_, s)| s.aen_config & AEN_CFG_NAMESPACE_ATTRIBUTE != 0)
            .map(|(&c, _)| c)
            .collect();
        for cntlid in targets {
            let state = inner
                .controllers
                .get_mut(&cntlid)
                .expect("cntlid was just listed under the same lock");
            state.changed_ns.insert(nsid);
            if let Some(parked) = state.parked.pop() {
                let _ = parked.tx.send(AerCompletion {
                    dw0: NAMESPACE_NOTICE_DW0,
                });
            }
        }
    }

    /// Read a controller's Changed Namespace List (LID 0x04) for a Get
    /// Log Page: return its changed NSIDs ascending. `consume` clears the
    /// set; `false` peeks without clearing — used when the host set RAE
    /// or supplied a buffer too short to hold the full page, so the
    /// change list isn't lost (issue #247). Empty when nothing changed
    /// since the last read / the controller is unknown — the dispatcher
    /// renders that as the all-zero page.
    pub fn take_changed_namespaces(&self, cntlid: u16, consume: bool) -> Vec<u32> {
        let mut inner = self.lock();
        match inner.controllers.get_mut(&cntlid) {
            Some(state) if consume => std::mem::take(&mut state.changed_ns).into_iter().collect(),
            Some(state) => state.changed_ns.iter().copied().collect(),
            None => Vec::new(),
        }
    }

    /// Store the controller-wide FID 0x0B Async Event Configuration.
    /// No-op if the controller is unknown.
    pub fn set_aen_config(&self, cntlid: u16, config: u32) {
        let mut inner = self.lock();
        if let Some(state) = inner.controllers.get_mut(&cntlid) {
            state.aen_config = config;
        }
    }

    /// Read the FID 0x0B Async Event Configuration (0 = no notices
    /// enabled, also the reply for an unknown controller).
    pub fn get_aen_config(&self, cntlid: u16) -> u32 {
        let inner = self.lock();
        inner
            .controllers
            .get(&cntlid)
            .map(|s| s.aen_config)
            .unwrap_or(0)
    }

    /// Store the FID 0x07 Number of Queues grant `(nsq, ncq)` for this
    /// controller. No-op if the controller is unknown (issue #245).
    pub fn set_num_io_queues(&self, cntlid: u16, nsq: u16, ncq: u16) {
        let mut inner = self.lock();
        if let Some(state) = inner.controllers.get_mut(&cntlid) {
            state.num_io_queues = Some((nsq, ncq));
        }
    }

    /// Read this controller's FID 0x07 grant `(nsq, ncq)`, or `None` if
    /// the host hasn't negotiated yet / the controller is unknown — the
    /// dispatcher then reports its cap (issue #245).
    pub fn get_num_io_queues(&self, cntlid: u16) -> Option<(u16, u16)> {
        let inner = self.lock();
        inner.controllers.get(&cntlid).and_then(|s| s.num_io_queues)
    }

    /// Store the FID 0x0F Keep Alive Timeout (ms) for this controller.
    /// No-op if the controller is unknown (issue #245).
    pub fn set_kato_ms(&self, cntlid: u16, ms: u32) {
        let mut inner = self.lock();
        if let Some(state) = inner.controllers.get_mut(&cntlid) {
            state.kato_ms = ms;
        }
    }

    /// Read this controller's FID 0x0F Keep Alive Timeout (ms); 0 when
    /// unset or the controller is unknown (issue #245).
    pub fn get_kato_ms(&self, cntlid: u16) -> u32 {
        let inner = self.lock();
        inner
            .controllers
            .get(&cntlid)
            .map(|s| s.kato_ms)
            .unwrap_or(0)
    }

    /// Live CNTLIDs of `host_id`, ascending. Empty when the host has no
    /// live controller.
    pub fn cntlids_for_host(&self, host_id: [u8; 16]) -> Vec<u16> {
        let inner = self.lock();
        let mut v: Vec<u16> = inner
            .controllers
            .iter()
            .filter(|(_, s)| s.host_id == host_id)
            .map(|(&c, _)| c)
            .collect();
        v.sort_unstable();
        v
    }

    /// A representative CNTLID for `host_id` (its lowest live one) for
    /// the Reservation Report. Returns 0 — never an assigned CNTLID —
    /// when the host has a persisted registration but no live controller
    /// (all its associations dropped; see #54).
    pub fn representative_cntlid(&self, host_id: [u8; 16]) -> u16 {
        let inner = self.lock();
        inner
            .controllers
            .iter()
            .filter(|(_, s)| s.host_id == host_id)
            .map(|(&c, _)| c)
            .min()
            .unwrap_or(0)
    }
}

impl Default for ControllerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ----------------------------------------------------------------
// Reservation-change sink (drives LID 0x80 + AER from the manager hook)
// ----------------------------------------------------------------

/// [`ReservationObserver`] that turns the shared manager's neutral
/// reservation changes into NVMe LID 0x80 + AER notifications. Registered
/// on the [`ReservationManager`](scsi_spc::reservations::ReservationManager)
/// at daemon boot; it fires regardless of which transport originated the
/// change (issue #67), so an iSCSI-issued preempt now reaches a fenced
/// NVMe host. The diff + issuer-exclusion live in the manager, so this
/// sink only self-filters to NVMe registrants and fans each out via the
/// unchanged [`ControllerRegistry::notify`].
pub struct AerReservationSink {
    registry: Arc<ControllerRegistry>,
}

impl AerReservationSink {
    pub fn new(registry: Arc<ControllerRegistry>) -> Self {
        Self { registry }
    }
}

impl ReservationObserver for AerReservationSink {
    fn on_reservation_change(&self, changes: &[ReservationChange]) {
        for change in changes {
            // iSCSI registrants are the SCSI UA sink's business.
            let RegistrantId::NvmeHost { hostid } = &change.affected else {
                continue;
            };
            // Inverse of `nsid_to_lun` (nsid = lun + 1). A LUN that can't
            // map back to a u32 NSID has no NVMe namespace, so skip it.
            let Ok(nsid) = u32::try_from(change.lun.saturating_add(1)) else {
                continue;
            };
            self.registry.notify(ReservationEvent {
                host_id: *hostid,
                nsid,
                kind: map_kind(change.kind),
            });
        }
    }
}

/// Bridge the neutral notification class onto the NVMe-wire event kind.
fn map_kind(kind: ReservationChangeKind) -> ReservationEventKind {
    match kind {
        ReservationChangeKind::RegistrationPreempted => ReservationEventKind::RegistrationPreempted,
        ReservationChangeKind::ReservationReleased => ReservationEventKind::ReservationReleased,
        ReservationChangeKind::ReservationPreempted => ReservationEventKind::ReservationPreempted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_A: [u8; 16] = [0xAA; 16];
    const HOST_B: [u8; 16] = [0xBB; 16];
    const NSID: u32 = 1;

    // -- ControllerRegistry --------------------------------------

    fn ev(host: [u8; 16], kind: ReservationEventKind) -> ReservationEvent {
        ReservationEvent {
            host_id: host,
            nsid: NSID,
            kind,
        }
    }

    #[test]
    fn connect_admin_allocates_distinct_cntlids() {
        let reg = ControllerRegistry::new();
        // Lowest-free allocation: 1, then 2.
        assert_eq!(reg.connect_admin(HOST_A).unwrap().cntlid(), 1);
        assert_eq!(reg.connect_admin(HOST_B).unwrap().cntlid(), 2);
    }

    #[test]
    fn connect_io_attaches_when_cntlid_matches() {
        let reg = ControllerRegistry::new();
        let adm = reg.connect_admin(HOST_A).unwrap();
        let io = reg.connect_io(HOST_A, adm.cntlid()).unwrap();
        assert_eq!(io.cntlid(), adm.cntlid());
    }

    #[test]
    fn connect_io_unknown_cntlid_errors() {
        let reg = ControllerRegistry::new();
        // No controller exists yet.
        assert_eq!(
            reg.connect_io(HOST_A, 1).unwrap_err(),
            ConnectError::UnknownController(1)
        );
        // CNTLID_ANY is never an assigned controller, so an I/O Connect
        // that forgot to name a real CNTLID is rejected.
        assert_eq!(
            reg.connect_io(HOST_A, nvme_base::fabrics::CNTLID_ANY)
                .unwrap_err(),
            ConnectError::UnknownController(nvme_base::fabrics::CNTLID_ANY)
        );
    }

    #[test]
    fn connect_io_host_mismatch_errors() {
        let reg = ControllerRegistry::new();
        let adm = reg.connect_admin(HOST_A).unwrap();
        // HOST_B may not attach an I/O queue to HOST_A's controller.
        assert_eq!(
            reg.connect_io(HOST_B, adm.cntlid()).unwrap_err(),
            ConnectError::HostMismatch(adm.cntlid())
        );
    }

    #[test]
    fn disconnect_frees_and_reuses_cntlid() {
        let reg = ControllerRegistry::new();
        let a = reg.connect_admin(HOST_A).unwrap();
        assert_eq!(a.cntlid(), 1);
        assert!(reg.disconnect(a), "last association frees the controller");
        assert!(reg.cntlids_for_host(HOST_A).is_empty());
        // CNTLID 1 is free again for the next admin Connect.
        assert_eq!(reg.connect_admin(HOST_B).unwrap().cntlid(), 1);
    }

    #[test]
    fn disconnect_keeps_controller_until_last_association() {
        let reg = ControllerRegistry::new();
        let adm = reg.connect_admin(HOST_A).unwrap();
        let io = reg.connect_io(HOST_A, adm.cntlid()).unwrap();
        // Dropping the I/O queue leaves the controller alive.
        assert!(!reg.disconnect(io));
        assert_eq!(reg.cntlids_for_host(HOST_A), vec![adm.cntlid()]);
        // Dropping the admin queue (the last association) frees it.
        assert!(reg.disconnect(adm));
        assert!(reg.cntlids_for_host(HOST_A).is_empty());
    }

    #[test]
    fn park_then_notify_fires_with_reservation_dw0() {
        let reg = ControllerRegistry::new();
        let token = reg.connect_admin(HOST_A).unwrap();
        let (tx, mut rx) = oneshot::channel();
        reg.park(token, tx);
        // No event yet → still parked.
        assert!(matches!(
            rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        reg.notify(ev(HOST_A, ReservationEventKind::RegistrationPreempted));
        let completion = rx.try_recv().expect("AER completed");
        assert_eq!(completion.dw0, RESERVATION_NOTICE_DW0);
    }

    #[test]
    fn notify_then_park_fires_immediately() {
        let reg = ControllerRegistry::new();
        let token = reg.connect_admin(HOST_A).unwrap();
        reg.notify(ev(HOST_A, ReservationEventKind::ReservationReleased));
        let (tx, mut rx) = oneshot::channel();
        reg.park(token, tx);
        assert_eq!(
            rx.try_recv().expect("immediate completion").dw0,
            RESERVATION_NOTICE_DW0
        );
    }

    #[test]
    fn take_log_entry_pops_oldest_and_reports_remaining() {
        let reg = ControllerRegistry::new();
        let c = reg.connect_admin(HOST_A).unwrap().cntlid();
        reg.notify(ev(HOST_A, ReservationEventKind::RegistrationPreempted));
        reg.notify(ReservationEvent {
            host_id: HOST_A,
            nsid: 7,
            kind: ReservationEventKind::ReservationPreempted,
        });
        // Oldest first: type 1, nsid 1, one more available.
        let first = reg.take_log_entry(c, true);
        assert_eq!(first[8], resv_notif_type::REGISTRATION_PREEMPTED);
        assert_eq!(first[9], 1);
        assert_eq!(u32::from_le_bytes(first[12..16].try_into().unwrap()), 1);
        assert_ne!(u64::from_le_bytes(first[0..8].try_into().unwrap()), 0);
        // Next: type 3, nsid 7, none remaining.
        let second = reg.take_log_entry(c, true);
        assert_eq!(second[8], resv_notif_type::RESERVATION_PREEMPTED);
        assert_eq!(second[9], 0);
        assert_eq!(u32::from_le_bytes(second[12..16].try_into().unwrap()), 7);
        // Drained → empty page.
        let empty = reg.take_log_entry(c, true);
        assert!(empty.iter().all(|&b| b == 0));
    }

    #[test]
    fn empty_queue_returns_zero_page() {
        let reg = ControllerRegistry::new();
        let c = reg.connect_admin(HOST_A).unwrap().cntlid();
        assert!(reg.take_log_entry(c, true).iter().all(|&b| b == 0));
        // An unknown controller also yields the empty page.
        assert!(reg.take_log_entry(0xABCD, true).iter().all(|&b| b == 0));
    }

    #[test]
    fn notify_fans_out_to_all_host_controllers() {
        let reg = ControllerRegistry::new();
        // One host, two controllers (two admin associations).
        let c1 = reg.connect_admin(HOST_A).unwrap().cntlid();
        let c2 = reg.connect_admin(HOST_A).unwrap().cntlid();
        assert_ne!(c1, c2);
        reg.notify(ev(HOST_A, ReservationEventKind::ReservationReleased));
        // Each controller has its own log entry.
        assert_eq!(
            reg.take_log_entry(c1, true)[8],
            resv_notif_type::RESERVATION_RELEASED
        );
        assert_eq!(
            reg.take_log_entry(c2, true)[8],
            resv_notif_type::RESERVATION_RELEASED
        );
    }

    #[test]
    fn notify_skips_other_hosts() {
        let reg = ControllerRegistry::new();
        let ca = reg.connect_admin(HOST_A).unwrap().cntlid();
        let cb = reg.connect_admin(HOST_B).unwrap().cntlid();
        reg.notify(ev(HOST_A, ReservationEventKind::ReservationReleased));
        assert_eq!(
            reg.take_log_entry(ca, true)[8],
            resv_notif_type::RESERVATION_RELEASED
        );
        // HOST_B's controller is untouched.
        assert!(reg.take_log_entry(cb, true).iter().all(|&b| b == 0));
    }

    #[test]
    fn masks_are_per_controller() {
        let reg = ControllerRegistry::new();
        // Same host, two controllers: masks are independent per CNTLID.
        let c1 = reg.connect_admin(HOST_A).unwrap().cntlid();
        let c2 = reg.connect_admin(HOST_A).unwrap().cntlid();
        reg.set_mask(c1, NSID, 1 << 2);
        assert_eq!(reg.get_mask(c1, NSID), 1 << 2);
        assert_eq!(
            reg.get_mask(c2, NSID),
            0,
            "sibling controller's mask is independent"
        );
    }

    #[test]
    fn mask_suppresses_notify_and_enqueue() {
        let reg = ControllerRegistry::new();
        let token = reg.connect_admin(HOST_A).unwrap();
        let c = token.cntlid();
        // Mask Registration Preempted (bit 1) for nsid 1.
        reg.set_mask(c, NSID, 1 << 1);
        assert_eq!(reg.get_mask(c, NSID), 1 << 1);
        let (tx, mut rx) = oneshot::channel();
        reg.park(token, tx);
        reg.notify(ev(HOST_A, ReservationEventKind::RegistrationPreempted));
        // Masked → no AER fired, no log entry queued.
        assert!(matches!(
            rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(reg.take_log_entry(c, true).iter().all(|&b| b == 0));
        // A different (unmasked) kind still fires.
        reg.notify(ev(HOST_A, ReservationEventKind::ReservationReleased));
        assert_eq!(
            rx.try_recv().expect("unmasked completion").dw0,
            RESERVATION_NOTICE_DW0
        );
    }

    #[test]
    fn disconnect_errors_parked_receiver() {
        let reg = ControllerRegistry::new();
        let token = reg.connect_admin(HOST_A).unwrap();
        let (tx, mut rx) = oneshot::channel();
        reg.park(token, tx);
        assert!(
            reg.disconnect(token),
            "last association frees the controller"
        );
        assert!(matches!(
            rx.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    #[test]
    fn log_and_masks_gone_after_controller_freed() {
        let reg = ControllerRegistry::new();
        let token = reg.connect_admin(HOST_A).unwrap();
        let c = token.cntlid();
        reg.set_mask(c, 9, 1 << 3);
        reg.notify(ev(HOST_A, ReservationEventKind::ReservationReleased));
        // Freeing the controller drops its log + masks (unlike the old
        // per-host model, where they survived a bare connection drop).
        assert!(reg.disconnect(token));
        // A reconnect reuses CNTLID 1 but starts clean.
        let again = reg.connect_admin(HOST_A).unwrap();
        assert_eq!(again.cntlid(), c);
        assert_eq!(
            reg.get_mask(c, 9),
            0,
            "masks did not survive controller free"
        );
        assert!(
            reg.take_log_entry(c, true).iter().all(|&b| b == 0),
            "log did not survive controller free"
        );
    }

    #[test]
    fn representative_cntlid_lowest_live_or_zero() {
        let reg = ControllerRegistry::new();
        // No live controller → 0 (the Reservation Report sentinel).
        assert_eq!(reg.representative_cntlid(HOST_A), 0);
        let c1 = reg.connect_admin(HOST_A).unwrap().cntlid();
        let c2 = reg.connect_admin(HOST_A).unwrap().cntlid();
        assert_eq!(reg.representative_cntlid(HOST_A), c1.min(c2));
    }

    // -- Namespace-attribute-changed notices (LID 0x04 / FID 0x0B) --

    /// Helper: enable namespace-change notices on a controller as the
    /// host would via Set Features FID 0x0B bit 8.
    fn enable_ns_notices(reg: &ControllerRegistry, cntlid: u16) {
        reg.set_aen_config(cntlid, AEN_CFG_NAMESPACE_ATTRIBUTE);
    }

    #[test]
    fn aen_config_round_trips_per_controller() {
        let reg = ControllerRegistry::new();
        let c1 = reg.connect_admin(HOST_A).unwrap().cntlid();
        let c2 = reg.connect_admin(HOST_B).unwrap().cntlid();
        // Default: no notices enabled.
        assert_eq!(reg.get_aen_config(c1), 0);
        reg.set_aen_config(c1, AEN_CFG_NAMESPACE_ATTRIBUTE);
        assert_eq!(reg.get_aen_config(c1), AEN_CFG_NAMESPACE_ATTRIBUTE);
        // Sibling controller is independent.
        assert_eq!(reg.get_aen_config(c2), 0);
        // Unknown controller reads 0.
        assert_eq!(reg.get_aen_config(0xABCD), 0);
    }

    #[test]
    fn namespace_notice_fans_to_all_enabled_hosts() {
        let reg = ControllerRegistry::new();
        // Two different hosts, both opted in. A namespace change is a
        // subsystem-wide event, so both see it (unlike reservations).
        let ca = reg.connect_admin(HOST_A).unwrap().cntlid();
        let cb = reg.connect_admin(HOST_B).unwrap().cntlid();
        enable_ns_notices(&reg, ca);
        enable_ns_notices(&reg, cb);
        reg.notify_namespace_attribute_changed(5);
        assert_eq!(reg.take_changed_namespaces(ca, true), vec![5]);
        assert_eq!(reg.take_changed_namespaces(cb, true), vec![5]);
    }

    #[test]
    fn namespace_notice_skips_controllers_that_did_not_opt_in() {
        let reg = ControllerRegistry::new();
        let optin = reg.connect_admin(HOST_A).unwrap().cntlid();
        let silent = reg.connect_admin(HOST_B).unwrap().cntlid();
        enable_ns_notices(&reg, optin);
        // `silent` never set FID 0x0B bit 8.
        reg.notify_namespace_attribute_changed(3);
        assert_eq!(reg.take_changed_namespaces(optin, true), vec![3]);
        assert!(reg.take_changed_namespaces(silent, true).is_empty());
    }

    #[test]
    fn namespace_notice_with_no_controllers_is_noop() {
        let reg = ControllerRegistry::new();
        // NVMe transport up but no host connected (or transport off):
        // the call simply finds no targets.
        reg.notify_namespace_attribute_changed(1);
        assert!(reg.take_changed_namespaces(1, true).is_empty());
    }

    #[test]
    fn park_then_namespace_notice_fires_with_namespace_dw0() {
        let reg = ControllerRegistry::new();
        let token = reg.connect_admin(HOST_A).unwrap();
        enable_ns_notices(&reg, token.cntlid());
        let (tx, mut rx) = oneshot::channel();
        reg.park(token, tx);
        assert!(matches!(
            rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        reg.notify_namespace_attribute_changed(9);
        assert_eq!(
            rx.try_recv().expect("AER completed").dw0,
            NAMESPACE_NOTICE_DW0
        );
    }

    #[test]
    fn namespace_notice_then_park_fires_immediately() {
        let reg = ControllerRegistry::new();
        let token = reg.connect_admin(HOST_A).unwrap();
        enable_ns_notices(&reg, token.cntlid());
        reg.notify_namespace_attribute_changed(2);
        let (tx, mut rx) = oneshot::channel();
        reg.park(token, tx);
        assert_eq!(
            rx.try_recv().expect("immediate completion").dw0,
            NAMESPACE_NOTICE_DW0
        );
    }

    #[test]
    fn take_changed_namespaces_dedups_sorts_and_clears() {
        let reg = ControllerRegistry::new();
        let c = reg.connect_admin(HOST_A).unwrap().cntlid();
        enable_ns_notices(&reg, c);
        // Out-of-order with a duplicate — the set collapses + sorts.
        reg.notify_namespace_attribute_changed(7);
        reg.notify_namespace_attribute_changed(1);
        reg.notify_namespace_attribute_changed(7);
        assert_eq!(reg.take_changed_namespaces(c, true), vec![1, 7]);
        // Drained — a second read is empty until the next change.
        assert!(reg.take_changed_namespaces(c, true).is_empty());
    }

    #[test]
    fn reservation_notice_takes_precedence_at_park() {
        // Both event types pending: park announces the reservation log
        // first; the namespace notice surfaces on the host's next AER.
        let reg = ControllerRegistry::new();
        let token = reg.connect_admin(HOST_A).unwrap();
        let c = token.cntlid();
        enable_ns_notices(&reg, c);
        reg.notify(ev(HOST_A, ReservationEventKind::ReservationReleased));
        reg.notify_namespace_attribute_changed(4);
        let (tx, mut rx) = oneshot::channel();
        reg.park(token, tx);
        assert_eq!(
            rx.try_recv().expect("first AER").dw0,
            RESERVATION_NOTICE_DW0
        );
        // Host drains the reservation log (Get Log Page LID 0x80); the
        // re-park then sees only the namespace change still queued.
        let _ = reg.take_log_entry(c, true);
        let (tx2, mut rx2) = oneshot::channel();
        reg.park(token, tx2);
        assert_eq!(
            rx2.try_recv().expect("second AER").dw0,
            NAMESPACE_NOTICE_DW0
        );
    }

    #[test]
    fn changed_ns_gone_after_controller_freed() {
        let reg = ControllerRegistry::new();
        let token = reg.connect_admin(HOST_A).unwrap();
        let c = token.cntlid();
        enable_ns_notices(&reg, c);
        reg.notify_namespace_attribute_changed(6);
        // Freeing the controller drops its changed list + AEN config.
        assert!(reg.disconnect(token));
        let again = reg.connect_admin(HOST_A).unwrap();
        assert_eq!(again.cntlid(), c);
        assert_eq!(reg.get_aen_config(c), 0, "AEN config did not survive free");
        assert!(
            reg.take_changed_namespaces(c, true).is_empty(),
            "changed list did not survive free"
        );
    }

    // -- AerReservationSink --------------------------------------
    //
    // The neutral diff + issuer-exclusion are unit-tested in
    // `scsi_spc::reservations`. Here we only prove the sink self-filters
    // to NVMe registrants, maps the LUN to nsid = lun + 1, and drives
    // `notify` (a LID 0x80 entry appears for the affected NVMe host).

    fn iscsi_id(tag: u8) -> RegistrantId {
        RegistrantId::iscsi(Some("iqn.test:x".into()), [tag; 6])
    }

    #[test]
    fn sink_notifies_nvme_host_and_skips_iscsi() {
        let reg = Arc::new(ControllerRegistry::new());
        let c = reg.connect_admin(HOST_A).unwrap().cntlid();
        let sink = AerReservationSink::new(Arc::clone(&reg));
        // A mixed change set on LUN 0 (nsid 1): the NVMe host A is
        // notified; the iSCSI registrant is the SCSI sink's job.
        sink.on_reservation_change(&[
            ReservationChange {
                lun: 0,
                affected: RegistrantId::nvme(HOST_A),
                kind: ReservationChangeKind::ReservationPreempted,
            },
            ReservationChange {
                lun: 0,
                affected: iscsi_id(7),
                kind: ReservationChangeKind::ReservationReleased,
            },
        ]);
        let page = reg.take_log_entry(c, true);
        assert_eq!(page[8], resv_notif_type::RESERVATION_PREEMPTED);
        assert_eq!(u32::from_le_bytes(page[12..16].try_into().unwrap()), 1); // nsid = lun + 1
        // Only the one (NVMe) entry was queued.
        assert!(reg.take_log_entry(c, true).iter().all(|&b| b == 0));
    }

    #[test]
    fn sink_maps_change_kind_to_event_kind() {
        let reg = Arc::new(ControllerRegistry::new());
        let c = reg.connect_admin(HOST_A).unwrap().cntlid();
        let sink = AerReservationSink::new(Arc::clone(&reg));
        sink.on_reservation_change(&[ReservationChange {
            lun: 6,
            affected: RegistrantId::nvme(HOST_A),
            kind: ReservationChangeKind::RegistrationPreempted,
        }]);
        let page = reg.take_log_entry(c, true);
        assert_eq!(page[8], resv_notif_type::REGISTRATION_PREEMPTED);
        assert_eq!(u32::from_le_bytes(page[12..16].try_into().unwrap()), 7);
    }

    // Issue #67 acceptance (one direction), end-to-end through the
    // manager observer: an iSCSI-issued PREEMPT fences an NVMe holder and
    // the AER sink fans a LID 0x80 entry to that host's controller.
    #[test]
    fn iscsi_preempt_notifies_nvme_host_via_observer() {
        use scsi_spc::pr::ReservationType;
        use scsi_spc::reservations::ReservationManager;

        let aer = Arc::new(ControllerRegistry::new());
        let mgr = ReservationManager::new();
        mgr.register_observer(Arc::new(AerReservationSink::new(Arc::clone(&aer))));

        // NVMe host A has a live controller and holds an exclusive reservation.
        let cntlid = aer.connect_admin(HOST_A).unwrap().cntlid();
        let nvme_a = RegistrantId::nvme(HOST_A);
        mgr.register(0, &nvme_a, 0, 0xAAAA, true, Some(false));
        mgr.reserve(0, &nvme_a, 0xAAAA, ReservationType::ExclusiveAccess.as_u8());

        // An iSCSI initiator B registers, then preempts A.
        let iscsi_b = iscsi_id(2);
        mgr.register(0, &iscsi_b, 0, 0xBBBB, true, Some(false));
        mgr.preempt(
            0,
            &iscsi_b,
            0xBBBB,
            0xAAAA,
            ReservationType::ExclusiveAccess.as_u8(),
        );

        let page = aer.take_log_entry(cntlid, true);
        assert_eq!(page[8], resv_notif_type::RESERVATION_PREEMPTED);
        assert_eq!(u32::from_le_bytes(page[12..16].try_into().unwrap()), 1);
    }

    /// Issue #245: Number of Queues + Keep Alive Timer feature state is
    /// per-controller — one controller's Set must not clobber another's.
    #[test]
    fn queue_and_kato_feature_state_is_per_controller() {
        let reg = ControllerRegistry::new();
        let c1 = reg.connect_admin(HOST_A).unwrap().cntlid();
        let c2 = reg.connect_admin(HOST_B).unwrap().cntlid();

        // Fresh controllers haven't negotiated — None / 0.
        assert_eq!(reg.get_num_io_queues(c1), None);
        assert_eq!(reg.get_kato_ms(c2), 0);

        reg.set_num_io_queues(c1, 3, 3);
        reg.set_kato_ms(c1, 10_000);
        // c2 is unaffected by c1's negotiation.
        assert_eq!(reg.get_num_io_queues(c1), Some((3, 3)));
        assert_eq!(reg.get_num_io_queues(c2), None);
        assert_eq!(reg.get_kato_ms(c1), 10_000);
        assert_eq!(reg.get_kato_ms(c2), 0);

        // c2 negotiates a different grant; c1 stays put.
        reg.set_num_io_queues(c2, 7, 7);
        assert_eq!(reg.get_num_io_queues(c1), Some((3, 3)));
        assert_eq!(reg.get_num_io_queues(c2), Some((7, 7)));
    }

    /// Issue #247: a RAE (peek) Get Log Page must NOT drain the
    /// per-controller event state — a later consuming read still sees it.
    #[test]
    fn rae_peek_does_not_consume_log_pages() {
        use scsi_spc::reservations::ReservationManager;
        let reg = Arc::new(ControllerRegistry::new());
        let c = reg.connect_admin(HOST_A).unwrap().cntlid();
        let mgr = ReservationManager::new();
        mgr.register_observer(Arc::new(AerReservationSink::new(Arc::clone(&reg))));

        // Queue a reservation notification + a changed-namespace entry.
        AerReservationSink::new(Arc::clone(&reg)).on_reservation_change(&[ReservationChange {
            lun: 0,
            affected: RegistrantId::nvme(HOST_A),
            kind: ReservationChangeKind::ReservationPreempted,
        }]);
        reg.set_aen_config(c, AEN_CFG_NAMESPACE_ATTRIBUTE);
        reg.notify_namespace_attribute_changed(42);

        // Peek (consume=false): the entries are reported but retained.
        let peek = reg.take_log_entry(c, false);
        assert_eq!(peek[8], resv_notif_type::RESERVATION_PREEMPTED);
        assert_eq!(reg.take_changed_namespaces(c, false), vec![42]);

        // A consuming read still sees them (peek didn't drain).
        let drained = reg.take_log_entry(c, true);
        assert_eq!(drained[8], resv_notif_type::RESERVATION_PREEMPTED);
        assert_eq!(reg.take_changed_namespaces(c, true), vec![42]);

        // Now they're gone.
        assert!(reg.take_log_entry(c, true).iter().all(|&b| b == 0));
        assert!(reg.take_changed_namespaces(c, true).is_empty());
    }
}
