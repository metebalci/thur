// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Asynchronous Event Request hub + reservation-notification event
//! derivation.
//!
//! [`AerHub`] is the per-controller runtime state shared (one
//! `Arc<AerHub>`) between the NVM dispatcher (the event *producer* —
//! reservation ops call [`AerHub::notify`], Get Log Page LID 0x80
//! calls [`AerHub::take_log_entry`], Set/Get Features FID 0x82 call
//! [`AerHub::set_mask`] / [`AerHub::get_mask`]) and the NVMe/TCP
//! transport (the event *consumer* — each connection's reader parks an
//! AER via [`AerHub::park`] and a delivery task awaits the oneshot).
//! This is what makes cross-connection delivery work: a reservation
//! command on host A's I/O queue completes an AER parked on host A's
//! admin queue, possibly on a different TCP connection.
//!
//! State is keyed by the 128-bit Connect HOSTID — the same key the
//! reservation manager uses for NVMe registrants. Parked AER senders
//! are *per-connection* (dropped on connection teardown via
//! [`AerHub::drop_conn`]); the notification log queue and the FID 0x82
//! masks are *per-host* controller state that survives reconnect,
//! mirroring how a host's reservation registration survives a
//! connection close.
//!
//! Routing is per-host today (CNTLID is a static 1). [`ConnToken`]
//! carries a `cntlid` placeholder: the seam where per-controller
//! CNTLID allocation (#56) will refine routing from per-host to
//! per-controller.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use nvme_base::aer::{AEI_RESERVATION_LOG_AVAILABLE, AET_IO_COMMAND_SET, async_event_dw0};
use nvme_base::log_page::{self, lid, resv_notif_type};
use scsi_spc::reservations::{RegistrantId, ReservationSnapshot};
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

/// Opaque per-connection handle minted by [`AerHub::register_conn`].
/// Names the controller association an AER is parked on. `host_id` is
/// the routing key today; `cntlid` is the #56 seam.
#[derive(Debug, Clone, Copy)]
pub struct ConnToken {
    id: u64,
    host_id: [u8; 16],
    // Static 1 until per-controller CNTLID allocation (#56) lands, at
    // which point routing keys on this instead of (only) host_id.
    #[allow(dead_code)]
    cntlid: u16,
}

struct ParkedAer {
    conn_id: u64,
    tx: oneshot::Sender<AerCompletion>,
}

#[derive(Default)]
struct HostState {
    /// Outstanding AERs across this host's admin connections.
    parked: Vec<ParkedAer>,
    /// Unread reservation notifications: `(log_page_count, type, nsid)`,
    /// oldest at the front.
    pending: VecDeque<(u64, u8, u32)>,
    /// Per-namespace FID 0x82 Reservation Notification Mask.
    masks: HashMap<u32, u32>,
}

struct Inner {
    hosts: HashMap<[u8; 16], HostState>,
}

/// Per-controller AER + reservation-notification state. Cheap to
/// share via `Arc`; all methods take `&self`.
pub struct AerHub {
    inner: Mutex<Inner>,
    next_conn_id: AtomicU64,
    /// Controller-global, monotonically increasing Log Page Count.
    /// Starts at 1 so 0 stays the "empty page" sentinel.
    next_log_count: AtomicU64,
}

impl AerHub {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                hosts: HashMap::new(),
            }),
            next_conn_id: AtomicU64::new(1),
            next_log_count: AtomicU64::new(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Mint a connection token for an admin association of `host_id`.
    pub fn register_conn(&self, host_id: [u8; 16]) -> ConnToken {
        let id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        ConnToken {
            id,
            host_id,
            cntlid: 1,
        }
    }

    /// Park an AER from a connection. If the host already has an
    /// unread notification, complete the AER immediately (announce
    /// availability) instead of parking it.
    pub fn park(&self, token: ConnToken, tx: oneshot::Sender<AerCompletion>) {
        let mut inner = self.lock();
        let state = inner.hosts.entry(token.host_id).or_default();
        if !state.pending.is_empty() {
            let _ = tx.send(AerCompletion {
                dw0: RESERVATION_NOTICE_DW0,
            });
            return;
        }
        state.parked.push(ParkedAer {
            conn_id: token.id,
            tx,
        });
    }

    /// Drop a connection's parked AERs (its oneshot senders), which
    /// unblocks any awaiting transport delivery tasks. Per-host
    /// notification log + masks are deliberately left intact.
    pub fn drop_conn(&self, token: ConnToken) {
        let mut inner = self.lock();
        if let Some(state) = inner.hosts.get_mut(&token.host_id) {
            state.parked.retain(|p| p.conn_id != token.id);
        }
    }

    /// Record a reservation event: mask-check, append to the host's
    /// log queue, and complete one parked AER (if any) to announce it.
    pub fn notify(&self, event: ReservationEvent) {
        let mut inner = self.lock();
        let state = inner.hosts.entry(event.host_id).or_default();
        let mask = state.masks.get(&event.nsid).copied().unwrap_or(0);
        if mask & event.kind.mask_bit() != 0 {
            return;
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

    /// Build the LID 0x80 page for a Get Log Page: pop the host's
    /// oldest unread notification (decrementing the available count)
    /// or return the all-zero empty page when the queue is drained.
    pub fn take_log_entry(
        &self,
        host_id: [u8; 16],
    ) -> [u8; log_page::RESERVATION_NOTIFICATION_LEN] {
        let mut inner = self.lock();
        let state = inner.hosts.entry(host_id).or_default();
        match state.pending.pop_front() {
            Some((count, type_byte, nsid)) => {
                let num_available = state.pending.len().min(u8::MAX as usize) as u8;
                log_page::reservation_notification(count, type_byte, num_available, nsid)
            }
            None => log_page::reservation_notification(0, resv_notif_type::EMPTY, 0, 0),
        }
    }

    /// Store the FID 0x82 Reservation Notification Mask for `(host, nsid)`.
    pub fn set_mask(&self, host_id: [u8; 16], nsid: u32, mask: u32) {
        let mut inner = self.lock();
        inner
            .hosts
            .entry(host_id)
            .or_default()
            .masks
            .insert(nsid, mask);
    }

    /// Read the FID 0x82 mask for `(host, nsid)` (0 = all enabled).
    pub fn get_mask(&self, host_id: [u8; 16], nsid: u32) -> u32 {
        let inner = self.lock();
        inner
            .hosts
            .get(&host_id)
            .and_then(|s| s.masks.get(&nsid).copied())
            .unwrap_or(0)
    }
}

impl Default for AerHub {
    fn default() -> Self {
        Self::new()
    }
}

// ----------------------------------------------------------------
// Reservation-event derivation (pure; no hub, no tokio)
// ----------------------------------------------------------------

/// The reservation mutation that just succeeded. Disambiguates the
/// notification class that two equivalent-looking snapshot diffs map
/// to (e.g. a removed registrant means Registration Preempted under a
/// Preempt but Reservation Preempted under a Clear).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResvAction {
    Preempt,
    Release,
    Clear,
    Unregister,
}

fn nvme_host(id: &RegistrantId) -> Option<[u8; 16]> {
    match id {
        RegistrantId::NvmeHost { hostid } => Some(*hostid),
        RegistrantId::Iscsi { .. } => None,
    }
}

fn emit(
    out: &mut Vec<ReservationEvent>,
    issuer: [u8; 16],
    host_id: [u8; 16],
    nsid: u32,
    kind: ReservationEventKind,
) {
    // The command issuer learns the result from its own completion; it
    // is never sent an asynchronous notification.
    if host_id != issuer {
        out.push(ReservationEvent {
            host_id,
            nsid,
            kind,
        });
    }
}

/// Derive the reservation notifications a just-completed (outcome
/// `Good`) reservation op generates, from the before/after snapshots,
/// the issuing host, and the action. Pure and table-testable.
///
/// Rules (NVMe NVM Command Set reservation notices):
/// - **Preempt**: the prior reservation holder that lost its
///   reservation → Reservation Preempted; any other registrant whose
///   registration was removed → Registration Preempted (the holder is
///   deduped to Reservation Preempted only, never both).
/// - **Release**: a reservation that existed and is now released with
///   no successor → Reservation Released to every other current
///   registrant. An all-registrants holder *rotation* keeps
///   `post.holder` set, so it emits nothing.
/// - **Clear**: every registration and the reservation are wiped →
///   Reservation Preempted to every other prior registrant.
/// - **Unregister** (self): only fans out if the departing host held a
///   non-all-registrants reservation that is now released → Reservation
///   Released to the survivors.
pub fn diff_reservation_events(
    action: ResvAction,
    pre: &ReservationSnapshot,
    post: &ReservationSnapshot,
    issuer: [u8; 16],
    nsid: u32,
) -> Vec<ReservationEvent> {
    let mut out = Vec::new();
    let is_present = |id: &RegistrantId| post.registrants.iter().any(|(p, _)| p == id);

    match action {
        ResvAction::Clear => {
            for (id, _) in &pre.registrants {
                if let Some(h) = nvme_host(id) {
                    emit(
                        &mut out,
                        issuer,
                        h,
                        nsid,
                        ReservationEventKind::ReservationPreempted,
                    );
                }
            }
        }
        ResvAction::Preempt => {
            // The prior holder whose reservation was taken over.
            let reservation_taken = pre.holder.is_some() && post.holder != pre.holder;
            let preempted_holder = if reservation_taken {
                pre.holder.as_ref().and_then(nvme_host)
            } else {
                None
            };
            if let Some(h) = preempted_holder {
                emit(
                    &mut out,
                    issuer,
                    h,
                    nsid,
                    ReservationEventKind::ReservationPreempted,
                );
            }
            // Registrants removed by the preempt that were not the
            // (already-notified) holder.
            for (id, _) in &pre.registrants {
                if is_present(id) {
                    continue;
                }
                if let Some(h) = nvme_host(id) {
                    if Some(h) == preempted_holder {
                        continue;
                    }
                    emit(
                        &mut out,
                        issuer,
                        h,
                        nsid,
                        ReservationEventKind::RegistrationPreempted,
                    );
                }
            }
        }
        ResvAction::Release | ResvAction::Unregister => {
            let released = pre.holder.is_some() && post.holder.is_none();
            if released {
                for (id, _) in &post.registrants {
                    if let Some(h) = nvme_host(id) {
                        emit(
                            &mut out,
                            issuer,
                            h,
                            nsid,
                            ReservationEventKind::ReservationReleased,
                        );
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_A: [u8; 16] = [0xAA; 16];
    const HOST_B: [u8; 16] = [0xBB; 16];
    const HOST_C: [u8; 16] = [0xCC; 16];
    const NSID: u32 = 1;

    fn snap(holder: Option<[u8; 16]>, regs: &[[u8; 16]]) -> ReservationSnapshot {
        ReservationSnapshot {
            generation: 0,
            reservation_type: None,
            holder: holder.map(RegistrantId::nvme),
            registrants: regs.iter().map(|h| (RegistrantId::nvme(*h), 1)).collect(),
        }
    }

    // -- AerHub --------------------------------------------------

    fn ev(host: [u8; 16], kind: ReservationEventKind) -> ReservationEvent {
        ReservationEvent {
            host_id: host,
            nsid: NSID,
            kind,
        }
    }

    #[test]
    fn park_then_notify_fires_with_reservation_dw0() {
        let hub = AerHub::new();
        let token = hub.register_conn(HOST_A);
        let (tx, mut rx) = oneshot::channel();
        hub.park(token, tx);
        // No event yet → still parked.
        assert!(matches!(
            rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        hub.notify(ev(HOST_A, ReservationEventKind::RegistrationPreempted));
        let completion = rx.try_recv().expect("AER completed");
        assert_eq!(completion.dw0, 0x0080_0006);
    }

    #[test]
    fn notify_then_park_fires_immediately() {
        let hub = AerHub::new();
        hub.notify(ev(HOST_A, ReservationEventKind::ReservationReleased));
        let token = hub.register_conn(HOST_A);
        let (tx, mut rx) = oneshot::channel();
        hub.park(token, tx);
        assert_eq!(
            rx.try_recv().expect("immediate completion").dw0,
            0x0080_0006
        );
    }

    #[test]
    fn take_log_entry_pops_oldest_and_reports_remaining() {
        let hub = AerHub::new();
        hub.notify(ev(HOST_A, ReservationEventKind::RegistrationPreempted));
        hub.notify(ReservationEvent {
            host_id: HOST_A,
            nsid: 7,
            kind: ReservationEventKind::ReservationPreempted,
        });
        // Oldest first: type 1, nsid 1, one more available.
        let first = hub.take_log_entry(HOST_A);
        assert_eq!(first[8], resv_notif_type::REGISTRATION_PREEMPTED);
        assert_eq!(first[9], 1);
        assert_eq!(u32::from_le_bytes(first[12..16].try_into().unwrap()), 1);
        assert_ne!(u64::from_le_bytes(first[0..8].try_into().unwrap()), 0);
        // Next: type 3, nsid 7, none remaining.
        let second = hub.take_log_entry(HOST_A);
        assert_eq!(second[8], resv_notif_type::RESERVATION_PREEMPTED);
        assert_eq!(second[9], 0);
        assert_eq!(u32::from_le_bytes(second[12..16].try_into().unwrap()), 7);
        // Drained → empty page.
        let empty = hub.take_log_entry(HOST_A);
        assert!(empty.iter().all(|&b| b == 0));
    }

    #[test]
    fn empty_queue_returns_zero_page() {
        let hub = AerHub::new();
        assert!(hub.take_log_entry(HOST_A).iter().all(|&b| b == 0));
    }

    #[test]
    fn mask_suppresses_notify_and_enqueue() {
        let hub = AerHub::new();
        // Mask Registration Preempted (bit 1) for nsid 1.
        hub.set_mask(HOST_A, NSID, 1 << 1);
        assert_eq!(hub.get_mask(HOST_A, NSID), 1 << 1);
        let token = hub.register_conn(HOST_A);
        let (tx, mut rx) = oneshot::channel();
        hub.park(token, tx);
        hub.notify(ev(HOST_A, ReservationEventKind::RegistrationPreempted));
        // Masked → no AER fired, no log entry queued.
        assert!(matches!(
            rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(hub.take_log_entry(HOST_A).iter().all(|&b| b == 0));
        // A different (unmasked) kind still fires.
        hub.notify(ev(HOST_A, ReservationEventKind::ReservationReleased));
        assert_eq!(rx.try_recv().expect("unmasked completion").dw0, 0x0080_0006);
    }

    #[test]
    fn drop_conn_errors_parked_receiver() {
        let hub = AerHub::new();
        let token = hub.register_conn(HOST_A);
        let (tx, mut rx) = oneshot::channel();
        hub.park(token, tx);
        hub.drop_conn(token);
        assert!(matches!(
            rx.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    #[test]
    fn log_and_masks_survive_drop_conn() {
        let hub = AerHub::new();
        let token = hub.register_conn(HOST_A);
        hub.set_mask(HOST_A, 9, 1 << 3);
        hub.notify(ev(HOST_A, ReservationEventKind::ReservationReleased));
        hub.drop_conn(token);
        // Mask and the queued entry both survive the connection drop.
        assert_eq!(hub.get_mask(HOST_A, 9), 1 << 3);
        assert_eq!(
            hub.take_log_entry(HOST_A)[8],
            resv_notif_type::RESERVATION_RELEASED
        );
    }

    // -- diff_reservation_events ---------------------------------

    #[test]
    fn preempt_non_holder_is_registration_preempted() {
        // B preempts A's registration; no reservation held.
        let pre = snap(None, &[HOST_A, HOST_B]);
        let post = snap(None, &[HOST_B]);
        let events = diff_reservation_events(ResvAction::Preempt, &pre, &post, HOST_B, NSID);
        assert_eq!(
            events,
            vec![ev(HOST_A, ReservationEventKind::RegistrationPreempted)]
        );
    }

    #[test]
    fn preempt_holder_is_reservation_preempted_only() {
        // A holds; B preempts and takes over, removing A's registration.
        // A must get Reservation Preempted (3) ONLY, never also type 1.
        let pre = snap(Some(HOST_A), &[HOST_A, HOST_B]);
        let post = snap(Some(HOST_B), &[HOST_B]);
        let events = diff_reservation_events(ResvAction::Preempt, &pre, &post, HOST_B, NSID);
        assert_eq!(
            events,
            vec![ev(HOST_A, ReservationEventKind::ReservationPreempted)]
        );
    }

    #[test]
    fn preempt_holder_plus_other_registrant() {
        // A holds, C also registered; B preempts taking over and
        // removing both A and C. A → type 3, C → type 1.
        let pre = snap(Some(HOST_A), &[HOST_A, HOST_B, HOST_C]);
        let post = snap(Some(HOST_B), &[HOST_B]);
        let events = diff_reservation_events(ResvAction::Preempt, &pre, &post, HOST_B, NSID);
        assert!(events.contains(&ev(HOST_A, ReservationEventKind::ReservationPreempted)));
        assert!(events.contains(&ev(HOST_C, ReservationEventKind::RegistrationPreempted)));
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn release_fans_out_reservation_released_to_others() {
        // A holds and releases; B and C remain registered.
        let pre = snap(Some(HOST_A), &[HOST_A, HOST_B, HOST_C]);
        let post = snap(None, &[HOST_A, HOST_B, HOST_C]);
        let events = diff_reservation_events(ResvAction::Release, &pre, &post, HOST_A, NSID);
        assert!(events.contains(&ev(HOST_B, ReservationEventKind::ReservationReleased)));
        assert!(events.contains(&ev(HOST_C, ReservationEventKind::ReservationReleased)));
        // Issuer A is never notified.
        assert!(!events.iter().any(|e| e.host_id == HOST_A));
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn self_unregister_holder_releases_to_survivors() {
        // A holds and unregisters; reservation releases, B survives.
        let pre = snap(Some(HOST_A), &[HOST_A, HOST_B]);
        let post = snap(None, &[HOST_B]);
        let events = diff_reservation_events(ResvAction::Unregister, &pre, &post, HOST_A, NSID);
        assert_eq!(
            events,
            vec![ev(HOST_B, ReservationEventKind::ReservationReleased)]
        );
    }

    #[test]
    fn clear_preempts_all_other_registrants() {
        let pre = snap(Some(HOST_A), &[HOST_A, HOST_B, HOST_C]);
        let post = snap(None, &[]);
        let events = diff_reservation_events(ResvAction::Clear, &pre, &post, HOST_A, NSID);
        assert!(events.contains(&ev(HOST_B, ReservationEventKind::ReservationPreempted)));
        assert!(events.contains(&ev(HOST_C, ReservationEventKind::ReservationPreempted)));
        assert!(!events.iter().any(|e| e.host_id == HOST_A));
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn all_registrants_holder_rotation_emits_nothing() {
        // All-registrants reservation: A (holder) unregisters, the
        // reservation rotates to B (post.holder still set) → no event.
        let pre = snap(Some(HOST_A), &[HOST_A, HOST_B]);
        let post = snap(Some(HOST_B), &[HOST_B]);
        let events = diff_reservation_events(ResvAction::Unregister, &pre, &post, HOST_A, NSID);
        assert!(events.is_empty());
    }

    #[test]
    fn idempotent_reacquire_emits_nothing() {
        // Re-acquire by the same holder, state unchanged.
        let pre = snap(Some(HOST_A), &[HOST_A, HOST_B]);
        let post = snap(Some(HOST_A), &[HOST_A, HOST_B]);
        let events = diff_reservation_events(ResvAction::Preempt, &pre, &post, HOST_A, NSID);
        assert!(events.is_empty());
    }
}
