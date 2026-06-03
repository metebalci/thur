// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)] // Session management infrastructure

use crate::error::IscsiError;
use crate::metrics;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{info, warn};

/// Session Manager - tracks all active iSCSI sessions
pub struct SessionManager {
    sessions: Arc<Mutex<HashMap<u16, Session>>>,
    next_tsih: Arc<Mutex<u16>>,
}

/// Session - represents an iSCSI session with one or more connections
pub struct Session {
    pub tsih: u16,
    pub isid: [u8; 6],
    pub connections: HashMap<u16, Connection>,
    pub created_at: Instant,
    pub last_activity: Instant,
    /// Logical partition this session is fenced to. Set after CHAP
    /// auth resolves the user's `partition` field. `None` = no
    /// fence (only legitimate when the library has no partitions
    /// defined; daemon refuses sessions otherwise).
    pub partition: Option<String>,
    /// Initiator IQN advertised at login. Recorded after the login
    /// completes (via [`SessionManager::set_initiator_iqn`]) so the
    /// reservation Unit-Attention sink can resolve a fenced registrant's
    /// `(IQN, ISID)` identity back to its live TSIH(s) (issue #67).
    pub initiator_iqn: Option<String>,
}

/// Connection - represents a single TCP connection within a session
pub struct Connection {
    pub cid: u16,
    /// Next non-immediate CmdSN we expect to see from the initiator.
    /// Lazy-initialized from the first non-immediate command in Full
    /// Feature Phase (the initiator picks the starting CmdSN — RFC
    /// 3720 §3.2.2.1) — `cmd_sn_initialized` flips true when we
    /// adopt that first value.
    pub exp_cmd_sn: u32,
    pub cmd_sn_initialized: bool,
    pub stat_sn: u32,
    pub max_recv_data_segment_length: u32,
}

/// Verdict returned by `check_cmdsn` for a non-immediate command PDU.
/// `Accept` advances the per-connection counter; `Duplicate` is a
/// retransmit at or just-below the expected sequence (we let the
/// caller process it again — full duplicate suppression would need a
/// per-CmdSN response cache); `OutOfWindow` is a wildly out-of-range
/// CmdSN — a malformed initiator (or wire replay) and the caller
/// must drop the connection rather than scramble tape semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdSnVerdict {
    Accept,
    Duplicate,
    OutOfWindow,
}

/// Validator-side CmdSN window — how far behind `exp_cmd_sn` an
/// incoming PDU's CmdSN may be and still be classified as a
/// duplicate (retransmit) rather than a protocol violation.
pub const CMDSN_WINDOW: u32 = 32;

/// Window we advertise to the initiator on every outbound response
/// (`MaxCmdSN - ExpCmdSN`). 32 lets the initiator pipeline up to 32
/// non-immediate Cmds without waiting for a response — required for
/// IOPS at wire-area latency and for the kernel iSCSI initiator's
/// natural multi-command pipelining (ext4 elevator, queue depth >1).
///
/// Safety: `serve_connection` runs a per-connection PDU reader task
/// that demuxes inbound PDUs by ITT into a per-Cmd Data-Out channel,
/// so a Cmd PDU racing in while we're awaiting Data-Out for an
/// earlier WRITE no longer trips `read_pdu`. The reader task forwards
/// fresh Cmd / NopOut / Logout PDUs to the dispatch loop while the
/// R2T waiter pulls from the per-ITT channel concurrently.
pub const ADVERTISED_CMDSN_WINDOW: u32 = 32;

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_tsih: Arc::new(Mutex::new(1)), // Start at 1 (0 is reserved for discovery)
        }
    }

    /// Create a new session with the given ISID
    pub fn create_session(&self, isid: [u8; 6]) -> u16 {
        let mut next_tsih = self
            .next_tsih
            .lock()
            .expect("session manager mutex poisoned");
        let tsih = *next_tsih;
        *next_tsih = next_tsih.wrapping_add(1);
        if *next_tsih == 0 {
            *next_tsih = 1; // Skip 0 (reserved)
        }
        drop(next_tsih);

        let session = Session {
            tsih,
            isid,
            connections: HashMap::new(),
            created_at: Instant::now(),
            last_activity: Instant::now(),
            partition: None,
            initiator_iqn: None,
        };

        let mut sessions = self
            .sessions
            .lock()
            .expect("session manager mutex poisoned");
        sessions.insert(tsih, session);
        let active = sessions.len() as i64;
        drop(sessions);
        metrics::record::sessions_active(active);
        info!("Created session TSIH={} ISID={:02x?}", tsih, isid);
        tsih
    }

    /// Add a connection to an existing session
    pub fn add_connection(
        &self,
        tsih: u16,
        cid: u16,
        max_recv_data_segment_length: u32,
    ) -> Result<(), IscsiError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| IscsiError::InvalidOp("session manager mutex poisoned"))?;
        let session = sessions
            .get_mut(&tsih)
            .ok_or(IscsiError::InvalidSession(tsih))?;

        let connection = Connection {
            cid,
            exp_cmd_sn: 0,
            cmd_sn_initialized: false,
            stat_sn: 1, // Start at 1
            max_recv_data_segment_length,
        };

        session.connections.insert(cid, connection);
        session.last_activity = Instant::now();
        info!(
            "Added connection CID={} to session TSIH={} (now {} connections)",
            cid,
            tsih,
            session.connections.len()
        );
        Ok(())
    }

    /// Remove a connection from a session
    pub fn remove_connection(&self, tsih: u16, cid: u16) -> Result<(), IscsiError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| IscsiError::InvalidOp("session manager mutex poisoned"))?;
        let session = sessions
            .get_mut(&tsih)
            .ok_or(IscsiError::InvalidSession(tsih))?;

        session.connections.remove(&cid);
        info!(
            "Removed connection CID={} from session TSIH={} ({} connections remaining)",
            cid,
            tsih,
            session.connections.len()
        );

        // If no more connections, remove session
        if session.connections.is_empty() {
            sessions.remove(&tsih);
            info!("Removed session TSIH={} (no more connections)", tsih);
        }
        let active = sessions.len() as i64;
        drop(sessions);
        metrics::record::sessions_active(active);

        Ok(())
    }

    /// Bind a session to a logical partition (set after CHAP auth
    /// resolves the user's `partition` field). Idempotent — re-setting
    /// to the same partition is a no-op; switching to a different
    /// partition is rejected (the binding is established at login
    /// time and immutable for the session's lifetime).
    pub fn set_partition(&self, tsih: u16, partition: Option<String>) -> Result<(), IscsiError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| IscsiError::InvalidOp("session manager mutex poisoned"))?;
        let session = sessions
            .get_mut(&tsih)
            .ok_or(IscsiError::InvalidSession(tsih))?;
        if let Some(existing) = &session.partition {
            if let Some(new) = &partition
                && existing != new
            {
                return Err(IscsiError::InvalidOp(
                    "session partition cannot change after login",
                ));
            }
            return Ok(());
        }
        session.partition = partition;
        Ok(())
    }

    /// Read the partition binding for a session, if any.
    pub fn partition_for(&self, tsih: u16) -> Option<String> {
        self.sessions
            .lock()
            .ok()
            .and_then(|s| s.get(&tsih).and_then(|sess| sess.partition.clone()))
    }

    /// Record the initiator IQN for a session (issue #67). Called once
    /// the login resolves the initiator name, mirroring [`set_partition`]
    /// so [`create_session`]'s signature stays stable. No-op for an
    /// unknown TSIH.
    ///
    /// [`set_partition`]: Self::set_partition
    /// [`create_session`]: Self::create_session
    pub fn set_initiator_iqn(&self, tsih: u16, iqn: Option<String>) {
        if let Ok(mut sessions) = self.sessions.lock()
            && let Some(session) = sessions.get_mut(&tsih)
        {
            session.initiator_iqn = iqn;
        }
    }

    /// Live TSIH(s) whose session matches a reservation registrant's
    /// `(IQN, ISID)` identity (issue #67). The reservation Unit-Attention
    /// sink uses this to target the affected initiators precisely.
    ///
    /// `collapse_isid` mirrors the PR initiator-port policy
    /// (`iscsi.reservations.initiator_port`): when set, a registrant's
    /// ISID is zeroed at the dispatcher, so the registrant carries no
    /// ISID and we match on IQN alone. When clear, both the IQN and the
    /// real wire ISID must match. A session whose IQN was never recorded
    /// never matches.
    pub fn tsihs_for(&self, iqn: Option<&str>, isid: [u8; 6], collapse_isid: bool) -> Vec<u16> {
        let sessions = self
            .sessions
            .lock()
            .expect("session manager mutex poisoned");
        sessions
            .values()
            .filter(|s| s.initiator_iqn.as_deref() == iqn && (collapse_isid || s.isid == isid))
            .map(|s| s.tsih)
            .collect()
    }

    /// Every live session's TSIH. Used to fan a logical-unit-wide Unit
    /// Attention (e.g. CAPACITY DATA HAS CHANGED after an online resize,
    /// issue #76) to all connected initiators — unlike
    /// [`tsihs_for`](Self::tsihs_for), which filters to one reservation
    /// registrant's identity, capacity change concerns every session.
    /// A UA queued for a session not admitted to the affected LUN is
    /// harmless: that nexus never issues commands on it, so it is never
    /// popped.
    pub fn active_tsihs(&self) -> Vec<u16> {
        let sessions = self
            .sessions
            .lock()
            .expect("session manager mutex poisoned");
        sessions.values().map(|s| s.tsih).collect()
    }

    /// Update session activity timestamp
    pub fn update_activity(&self, tsih: u16) -> Result<(), IscsiError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| IscsiError::InvalidOp("session manager mutex poisoned"))?;
        let session = sessions
            .get_mut(&tsih)
            .ok_or(IscsiError::InvalidSession(tsih))?;
        session.last_activity = Instant::now();
        Ok(())
    }

    /// Check an incoming PDU's CmdSN against the per-connection window
    /// (RFC 3720 §3.2.2.1). Immediate PDUs do not consume CmdSN. The
    /// first non-immediate PDU in Full Feature Phase seeds
    /// `exp_cmd_sn` from its own CmdSN — initiators pick the starting
    /// value. Subsequent PDUs must land in
    /// `[exp_cmd_sn - CMDSN_WINDOW, exp_cmd_sn + CMDSN_WINDOW]`
    /// modulo `u32` wrap; outside that range is a protocol violation.
    /// Strict in-order CmdSN advance: anything past `exp_cmd_sn` is
    /// also a violation (we don't queue future PDUs — single-LUN-per-
    /// session use cases don't need it). Returns `Duplicate` for
    /// retransmits at-or-below `exp_cmd_sn`; we still process them
    /// (a full duplicate-suppression cache is out of scope here).
    pub fn check_cmdsn(
        &self,
        tsih: u16,
        cid: u16,
        cmdsn: u32,
        immediate: bool,
    ) -> Result<CmdSnVerdict, IscsiError> {
        if immediate {
            return Ok(CmdSnVerdict::Accept);
        }
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| IscsiError::InvalidOp("session manager mutex poisoned"))?;
        let session = sessions
            .get_mut(&tsih)
            .ok_or(IscsiError::InvalidSession(tsih))?;
        let connection = session
            .connections
            .get_mut(&cid)
            .ok_or(IscsiError::InvalidSession(tsih))?;

        if !connection.cmd_sn_initialized {
            connection.exp_cmd_sn = cmdsn.wrapping_add(1);
            connection.cmd_sn_initialized = true;
            return Ok(CmdSnVerdict::Accept);
        }

        let exp = connection.exp_cmd_sn;
        // Modular distance: how far ahead `cmdsn` is from `exp` modulo
        // u32. If <= CMDSN_WINDOW it's a future PDU within the window;
        // if > CMDSN_WINDOW it could be a duplicate (large positive ==
        // small negative under u32 wrap — see below).
        let ahead = cmdsn.wrapping_sub(exp);
        // Symmetric backward distance for duplicate detection.
        let behind = exp.wrapping_sub(cmdsn);

        if ahead == 0 {
            connection.exp_cmd_sn = exp.wrapping_add(1);
            return Ok(CmdSnVerdict::Accept);
        }
        if ahead <= CMDSN_WINDOW {
            // Within the window but ahead of exp — gap. Strict mode:
            // refuse rather than queue.
            return Ok(CmdSnVerdict::OutOfWindow);
        }
        if behind <= CMDSN_WINDOW {
            // Retransmit at-or-just-below exp_cmd_sn.
            return Ok(CmdSnVerdict::Duplicate);
        }
        Ok(CmdSnVerdict::OutOfWindow)
    }

    /// Read the current `exp_cmd_sn` for a connection. Used to stamp
    /// outbound responses' ExpCmdSN/MaxCmdSN fields (advertising flow
    /// control to the initiator).
    pub fn current_exp_cmd_sn(&self, tsih: u16, cid: u16) -> Result<u32, IscsiError> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| IscsiError::InvalidOp("session manager mutex poisoned"))?;
        let session = sessions
            .get(&tsih)
            .ok_or(IscsiError::InvalidSession(tsih))?;
        let connection = session
            .connections
            .get(&cid)
            .ok_or(IscsiError::InvalidSession(tsih))?;
        Ok(connection.exp_cmd_sn)
    }

    /// Get StatSN for a connection and increment it
    pub fn get_and_increment_statsn(&self, tsih: u16, cid: u16) -> Result<u32, IscsiError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| IscsiError::InvalidOp("session manager mutex poisoned"))?;
        let session = sessions
            .get_mut(&tsih)
            .ok_or(IscsiError::InvalidSession(tsih))?;

        let connection = session
            .connections
            .get_mut(&cid)
            .ok_or(IscsiError::InvalidSession(tsih))?;

        let statsn = connection.stat_sn;
        connection.stat_sn = connection.stat_sn.wrapping_add(1);
        Ok(statsn)
    }

    /// Peek the current StatSN without advancing it. RFC 3720 §10.8: an
    /// R2T carries the StatSN that the next Status-bearing PDU will
    /// use, and itself does not consume one. Same applies to Data-In
    /// PDUs with S=0.
    pub fn current_stat_sn(&self, tsih: u16, cid: u16) -> Result<u32, IscsiError> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| IscsiError::InvalidOp("session manager mutex poisoned"))?;
        let session = sessions
            .get(&tsih)
            .ok_or(IscsiError::InvalidSession(tsih))?;
        let connection = session
            .connections
            .get(&cid)
            .ok_or(IscsiError::InvalidSession(tsih))?;
        Ok(connection.stat_sn)
    }

    /// Check if a session exists
    pub fn session_exists(&self, tsih: u16) -> bool {
        let sessions = self
            .sessions
            .lock()
            .expect("session manager mutex poisoned");
        sessions.contains_key(&tsih)
    }

    /// Get session count
    pub fn session_count(&self) -> usize {
        let sessions = self
            .sessions
            .lock()
            .expect("session manager mutex poisoned");
        sessions.len()
    }

    /// Get total connection count across all sessions
    pub fn connection_count(&self) -> usize {
        let sessions = self
            .sessions
            .lock()
            .expect("session manager mutex poisoned");
        sessions.values().map(|s| s.connections.len()).sum()
    }

    /// Clean up stale sessions (idle > timeout_seconds)
    pub fn cleanup_stale_sessions(&self, timeout_seconds: u64) {
        let mut sessions = self
            .sessions
            .lock()
            .expect("session manager mutex poisoned");
        let now = Instant::now();

        sessions.retain(|tsih, session| {
            let idle_secs = now.duration_since(session.last_activity).as_secs();
            if idle_secs > timeout_seconds {
                warn!(
                    "Removing stale session TSIH={} (idle for {}s)",
                    tsih, idle_secs
                );
                false
            } else {
                true
            }
        });
        let active = sessions.len() as i64;
        drop(sessions);
        metrics::record::sessions_active(active);
    }

    /// Get session info for monitoring
    pub fn get_session_info(&self) -> Vec<SessionInfo> {
        let sessions = self
            .sessions
            .lock()
            .expect("session manager mutex poisoned");
        sessions
            .values()
            .map(|s| SessionInfo {
                tsih: s.tsih,
                isid: s.isid,
                connection_count: s.connections.len(),
                age_seconds: s.created_at.elapsed().as_secs(),
                idle_seconds: s.last_activity.elapsed().as_secs(),
            })
            .collect()
    }
}

/// Session info for monitoring/debugging
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionInfo {
    pub tsih: u16,
    pub isid: [u8; 6],
    pub connection_count: usize,
    pub age_seconds: u64,
    pub idle_seconds: u64,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_session() {
        let mgr = SessionManager::new();
        let isid = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
        let tsih = mgr.create_session(isid);
        assert_eq!(tsih, 1);
        assert!(mgr.session_exists(tsih));
    }

    #[test]
    fn test_add_remove_connection() {
        let mgr = SessionManager::new();
        let isid = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
        let tsih = mgr.create_session(isid);

        // Add connection
        assert!(mgr.add_connection(tsih, 0, 131072).is_ok());
        assert_eq!(mgr.connection_count(), 1);

        // Remove connection
        assert!(mgr.remove_connection(tsih, 0).is_ok());
        assert_eq!(mgr.connection_count(), 0);
        assert!(!mgr.session_exists(tsih)); // Session should be removed
    }

    #[test]
    fn test_multiple_sessions() {
        let mgr = SessionManager::new();
        let isid1 = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
        let isid2 = [0x00, 0x11, 0x12, 0x13, 0x14, 0x15];

        let tsih1 = mgr.create_session(isid1);
        let tsih2 = mgr.create_session(isid2);

        assert_ne!(tsih1, tsih2);
        assert_eq!(mgr.session_count(), 2);

        mgr.add_connection(tsih1, 0, 131072).unwrap();
        mgr.add_connection(tsih2, 0, 131072).unwrap();

        assert_eq!(mgr.connection_count(), 2);
    }

    #[test]
    fn active_tsihs_returns_every_live_session() {
        let mgr = SessionManager::new();
        assert!(mgr.active_tsihs().is_empty());
        let t1 = mgr.create_session([0, 1, 2, 3, 4, 5]);
        let t2 = mgr.create_session([0, 6, 7, 8, 9, 10]);
        let mut got = mgr.active_tsihs();
        got.sort_unstable();
        let mut want = [t1, t2];
        want.sort_unstable();
        assert_eq!(got, want);
    }

    #[test]
    fn test_statsn_increment() {
        let mgr = SessionManager::new();
        let isid = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
        let tsih = mgr.create_session(isid);
        mgr.add_connection(tsih, 0, 131072).unwrap();

        let sn1 = mgr.get_and_increment_statsn(tsih, 0).unwrap();
        let sn2 = mgr.get_and_increment_statsn(tsih, 0).unwrap();
        let sn3 = mgr.get_and_increment_statsn(tsih, 0).unwrap();

        assert_eq!(sn1, 1);
        assert_eq!(sn2, 2);
        assert_eq!(sn3, 3);
    }

    #[test]
    fn test_invalid_session() {
        let mgr = SessionManager::new();
        let result = mgr.add_connection(9999, 0, 131072);
        assert!(result.is_err());
    }

    #[test]
    fn test_cmdsn_initial_seed_and_in_order() {
        let mgr = SessionManager::new();
        let isid = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
        let tsih = mgr.create_session(isid);
        mgr.add_connection(tsih, 0, 131072).unwrap();

        // Initiator can pick any starting CmdSN.
        assert_eq!(
            mgr.check_cmdsn(tsih, 0, 0x1000, false).unwrap(),
            CmdSnVerdict::Accept
        );
        assert_eq!(mgr.current_exp_cmd_sn(tsih, 0).unwrap(), 0x1001);
        assert_eq!(
            mgr.check_cmdsn(tsih, 0, 0x1001, false).unwrap(),
            CmdSnVerdict::Accept
        );
        assert_eq!(mgr.current_exp_cmd_sn(tsih, 0).unwrap(), 0x1002);
    }

    #[test]
    fn test_cmdsn_immediate_does_not_advance() {
        let mgr = SessionManager::new();
        let isid = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
        let tsih = mgr.create_session(isid);
        mgr.add_connection(tsih, 0, 131072).unwrap();

        // Immediate PDUs bypass — they don't consume CmdSN nor seed it.
        assert_eq!(
            mgr.check_cmdsn(tsih, 0, 0xDEAD_BEEF, true).unwrap(),
            CmdSnVerdict::Accept
        );
        // First non-immediate still seeds.
        assert_eq!(
            mgr.check_cmdsn(tsih, 0, 0x1000, false).unwrap(),
            CmdSnVerdict::Accept
        );
        assert_eq!(mgr.current_exp_cmd_sn(tsih, 0).unwrap(), 0x1001);
    }

    #[test]
    fn test_cmdsn_out_of_window_rejected() {
        let mgr = SessionManager::new();
        let isid = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
        let tsih = mgr.create_session(isid);
        mgr.add_connection(tsih, 0, 131072).unwrap();

        mgr.check_cmdsn(tsih, 0, 0x1000, false).unwrap();
        // Far-ahead CmdSN: a malformed initiator scrambling tape semantics.
        assert_eq!(
            mgr.check_cmdsn(tsih, 0, 0x1000 + CMDSN_WINDOW + 1, false)
                .unwrap(),
            CmdSnVerdict::OutOfWindow
        );
        // exp_cmd_sn must not advance on rejection.
        assert_eq!(mgr.current_exp_cmd_sn(tsih, 0).unwrap(), 0x1001);
    }

    #[test]
    fn test_cmdsn_duplicate_detected() {
        let mgr = SessionManager::new();
        let isid = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
        let tsih = mgr.create_session(isid);
        mgr.add_connection(tsih, 0, 131072).unwrap();

        mgr.check_cmdsn(tsih, 0, 0x1000, false).unwrap();
        mgr.check_cmdsn(tsih, 0, 0x1001, false).unwrap();
        // Retransmit of a previously-seen CmdSN.
        assert_eq!(
            mgr.check_cmdsn(tsih, 0, 0x1000, false).unwrap(),
            CmdSnVerdict::Duplicate
        );
        assert_eq!(mgr.current_exp_cmd_sn(tsih, 0).unwrap(), 0x1002);
    }

    #[test]
    fn test_tsihs_for_matches_iqn_and_isid() {
        let mgr = SessionManager::new();
        let isid_a = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
        let isid_b = [0x10, 0x11, 0x12, 0x13, 0x14, 0x15];
        let ta = mgr.create_session(isid_a);
        let tb = mgr.create_session(isid_b);
        mgr.set_initiator_iqn(ta, Some("iqn.test:a".into()));
        mgr.set_initiator_iqn(tb, Some("iqn.test:b".into()));

        // Non-collapse: IQN + ISID must both match.
        assert_eq!(mgr.tsihs_for(Some("iqn.test:a"), isid_a, false), vec![ta]);
        // Same IQN, different ISID => no match in non-collapse mode.
        assert!(mgr.tsihs_for(Some("iqn.test:a"), isid_b, false).is_empty());
        // Unknown IQN matches nothing.
        assert!(mgr.tsihs_for(Some("iqn.test:z"), isid_a, false).is_empty());
        let _ = tb;
    }

    #[test]
    fn test_tsihs_for_collapse_matches_iqn_only() {
        let mgr = SessionManager::new();
        // Two sessions, same IQN, different (real wire) ISIDs — as MPIO
        // paths look. In collapse mode the registrant's ISID is zeroed,
        // so both live paths must be returned for the IQN.
        let t1 = mgr.create_session([1u8; 6]);
        let t2 = mgr.create_session([2u8; 6]);
        mgr.set_initiator_iqn(t1, Some("iqn.test:mpio".into()));
        mgr.set_initiator_iqn(t2, Some("iqn.test:mpio".into()));
        let mut got = mgr.tsihs_for(Some("iqn.test:mpio"), [0u8; 6], true);
        got.sort_unstable();
        let mut want = vec![t1, t2];
        want.sort_unstable();
        assert_eq!(got, want);
    }

    #[test]
    fn test_tsihs_for_skips_sessions_without_recorded_iqn() {
        let mgr = SessionManager::new();
        let t = mgr.create_session([1u8; 6]);
        // No set_initiator_iqn => never matches an IQN-bearing query.
        assert!(
            mgr.tsihs_for(Some("iqn.test:a"), [1u8; 6], false)
                .is_empty()
        );
        let _ = t;
    }

    #[test]
    fn test_cmdsn_future_in_window_rejected_strict() {
        let mgr = SessionManager::new();
        let isid = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
        let tsih = mgr.create_session(isid);
        mgr.add_connection(tsih, 0, 131072).unwrap();

        mgr.check_cmdsn(tsih, 0, 0x1000, false).unwrap();
        // Gap of 1 — within the window but ahead of exp. Strict mode
        // rejects rather than queueing future PDUs.
        assert_eq!(
            mgr.check_cmdsn(tsih, 0, 0x1002, false).unwrap(),
            CmdSnVerdict::OutOfWindow
        );
        assert_eq!(mgr.current_exp_cmd_sn(tsih, 0).unwrap(), 0x1001);
    }
}
