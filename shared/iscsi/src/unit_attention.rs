// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Unit Attention tracking for multi-LUN iSCSI target
//
// Unit Attention is a SCSI condition that signals significant events to initiators:
// - Media changes (cartridge loaded/unloaded)
// - Power-on/reset

#![allow(dead_code)] // Unit attention infrastructure
// - Mode parameters changed
// - Inventory changes (for medium changer)
//
// Key requirements:
// - Track Unit Attention per session (TSIH) AND per LUN
// - Each LUN can have different pending UAs
// - UA is cleared after being reported once
// - Multiple UAs can be queued per session+LUN

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{debug, info};

/// Unit Attention ASC/ASCQ codes (from SCSI spec)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitAttentionCode {
    pub asc: u8,
    pub ascq: u8,
}

impl UnitAttentionCode {
    // Power-on, reset, or bus device reset occurred
    pub const POWER_ON_RESET: Self = Self {
        asc: 0x29,
        ascq: 0x00,
    };

    // Not ready to ready transition (media may have changed)
    pub const MEDIUM_MAY_HAVE_CHANGED: Self = Self {
        asc: 0x28,
        ascq: 0x00,
    };

    // Parameters changed
    pub const MODE_PARAMETERS_CHANGED: Self = Self {
        asc: 0x2A,
        ascq: 0x01,
    };

    // Microcode has been changed (not used in VTL)
    pub const MICROCODE_CHANGED: Self = Self {
        asc: 0x3F,
        ascq: 0x01,
    };

    // I_T nexus loss (initiator-target) - connection dropped
    pub const I_T_NEXUS_LOSS: Self = Self {
        asc: 0x29,
        ascq: 0x07,
    };

    // Inquiry data has changed (for REPORT LUNS updates)
    pub const INQUIRY_DATA_CHANGED: Self = Self {
        asc: 0x3F,
        ascq: 0x03,
    };

    // Reported LUNs data has changed
    pub const REPORTED_LUNS_DATA_CHANGED: Self = Self {
        asc: 0x3F,
        ascq: 0x0E,
    };
}

/// Map key: (session TSIH, target LUN). One UA list per nexus.
type UaKey = (u16, u8);
type UaMap = Arc<Mutex<HashMap<UaKey, Vec<UnitAttentionCode>>>>;

/// Unit Attention tracker - manages pending UAs per (session, LUN) tuple
pub struct UnitAttentionTracker {
    pending_ua: UaMap,
}

impl UnitAttentionTracker {
    pub fn new() -> Self {
        Self {
            pending_ua: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Add a Unit Attention condition for a specific session and LUN
    pub fn add_ua(&self, tsih: u16, lun: u8, code: UnitAttentionCode) {
        let mut pending = self.pending_ua.lock().expect("UA tracker mutex poisoned");
        pending.entry((tsih, lun)).or_default().push(code);
        info!(
            "Added Unit Attention for TSIH={} LUN={}: ASC=0x{:02x} ASCQ=0x{:02x}",
            tsih, lun, code.asc, code.ascq
        );
    }

    /// Add a Unit Attention for all sessions on a specific LUN
    /// (e.g., when media is loaded into a drive, all connected initiators should know)
    pub fn add_ua_all_sessions(&self, lun: u8, code: UnitAttentionCode) {
        let mut pending = self.pending_ua.lock().expect("UA tracker mutex poisoned");

        // Find all unique TSIHs
        let sessions: Vec<u16> = pending
            .keys()
            .map(|(tsih, _)| *tsih)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        // Add UA for each session on this LUN
        for tsih in sessions {
            pending.entry((tsih, lun)).or_default().push(code);
            debug!(
                "Added Unit Attention (all sessions) for TSIH={} LUN={}: ASC=0x{:02x} ASCQ=0x{:02x}",
                tsih, lun, code.asc, code.ascq
            );
        }

        info!(
            "Added Unit Attention for all sessions on LUN {}: ASC=0x{:02x} ASCQ=0x{:02x}",
            lun, code.asc, code.ascq
        );
    }

    /// Check and pop the next pending Unit Attention for a session+LUN
    /// Returns None if no pending UA
    pub fn check_and_pop_ua(&self, tsih: u16, lun: u8) -> Option<UnitAttentionCode> {
        let mut pending = self.pending_ua.lock().expect("UA tracker mutex poisoned");
        if let Some(uas) = pending.get_mut(&(tsih, lun))
            && !uas.is_empty()
        {
            let code = uas.remove(0); // Pop first UA
            info!(
                "Returning Unit Attention for TSIH={} LUN={}: ASC=0x{:02x} ASCQ=0x{:02x}",
                tsih, lun, code.asc, code.ascq
            );
            return Some(code);
        }
        None
    }

    /// Check if there's a pending UA without popping it
    pub fn has_pending_ua(&self, tsih: u16, lun: u8) -> bool {
        let pending = self.pending_ua.lock().expect("UA tracker mutex poisoned");
        pending
            .get(&(tsih, lun))
            .map(|uas| !uas.is_empty())
            .unwrap_or(false)
    }

    /// Clear all Unit Attentions for a session (all LUNs)
    /// Used when session is terminated
    pub fn clear_session(&self, tsih: u16) {
        let mut pending = self.pending_ua.lock().expect("UA tracker mutex poisoned");
        pending.retain(|(t, _), _| *t != tsih);
        info!("Cleared all Unit Attentions for session TSIH={}", tsih);
    }

    /// Clear all Unit Attentions for a specific session+LUN
    pub fn clear_lun(&self, tsih: u16, lun: u8) {
        let mut pending = self.pending_ua.lock().expect("UA tracker mutex poisoned");
        pending.remove(&(tsih, lun));
        debug!("Cleared Unit Attentions for TSIH={} LUN={}", tsih, lun);
    }

    /// Initialize a session - add power-on/reset UA for all LUNs
    pub fn initialize_session(&self, tsih: u16, num_luns: u8) {
        for lun in 0..num_luns {
            self.add_ua(tsih, lun, UnitAttentionCode::POWER_ON_RESET);
        }
    }
}

impl Default for UnitAttentionTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_check_ua() {
        let tracker = UnitAttentionTracker::new();

        // Add UA for session 1, LUN 1
        tracker.add_ua(1, 1, UnitAttentionCode::MEDIUM_MAY_HAVE_CHANGED);

        // Check it's pending
        assert!(tracker.has_pending_ua(1, 1));
        assert!(!tracker.has_pending_ua(1, 0)); // Different LUN
        assert!(!tracker.has_pending_ua(2, 1)); // Different session

        // Pop and verify
        let ua = tracker.check_and_pop_ua(1, 1);
        assert!(ua.is_some());
        assert_eq!(ua.unwrap(), UnitAttentionCode::MEDIUM_MAY_HAVE_CHANGED);

        // Should be cleared now
        assert!(!tracker.has_pending_ua(1, 1));
    }

    #[test]
    fn test_multiple_uas_per_lun() {
        let tracker = UnitAttentionTracker::new();

        // Add multiple UAs
        tracker.add_ua(1, 1, UnitAttentionCode::POWER_ON_RESET);
        tracker.add_ua(1, 1, UnitAttentionCode::MEDIUM_MAY_HAVE_CHANGED);
        tracker.add_ua(1, 1, UnitAttentionCode::MODE_PARAMETERS_CHANGED);

        // Pop them in order
        assert_eq!(
            tracker.check_and_pop_ua(1, 1),
            Some(UnitAttentionCode::POWER_ON_RESET)
        );
        assert_eq!(
            tracker.check_and_pop_ua(1, 1),
            Some(UnitAttentionCode::MEDIUM_MAY_HAVE_CHANGED)
        );
        assert_eq!(
            tracker.check_and_pop_ua(1, 1),
            Some(UnitAttentionCode::MODE_PARAMETERS_CHANGED)
        );
        assert_eq!(tracker.check_and_pop_ua(1, 1), None);
    }

    #[test]
    fn test_per_lun_isolation() {
        let tracker = UnitAttentionTracker::new();

        // Add UAs to different LUNs
        tracker.add_ua(1, 0, UnitAttentionCode::POWER_ON_RESET);
        tracker.add_ua(1, 1, UnitAttentionCode::MEDIUM_MAY_HAVE_CHANGED);
        tracker.add_ua(1, 2, UnitAttentionCode::MODE_PARAMETERS_CHANGED);

        // Each LUN should have its own UA
        assert_eq!(
            tracker.check_and_pop_ua(1, 0),
            Some(UnitAttentionCode::POWER_ON_RESET)
        );
        assert_eq!(
            tracker.check_and_pop_ua(1, 1),
            Some(UnitAttentionCode::MEDIUM_MAY_HAVE_CHANGED)
        );
        assert_eq!(
            tracker.check_and_pop_ua(1, 2),
            Some(UnitAttentionCode::MODE_PARAMETERS_CHANGED)
        );
    }

    #[test]
    fn test_clear_session() {
        let tracker = UnitAttentionTracker::new();

        // Add UAs for multiple LUNs
        tracker.add_ua(1, 0, UnitAttentionCode::POWER_ON_RESET);
        tracker.add_ua(1, 1, UnitAttentionCode::MEDIUM_MAY_HAVE_CHANGED);
        tracker.add_ua(2, 0, UnitAttentionCode::MODE_PARAMETERS_CHANGED);

        // Clear session 1
        tracker.clear_session(1);

        // Session 1 should be cleared
        assert!(!tracker.has_pending_ua(1, 0));
        assert!(!tracker.has_pending_ua(1, 1));

        // Session 2 should still have UA
        assert!(tracker.has_pending_ua(2, 0));
    }

    #[test]
    fn test_initialize_session() {
        let tracker = UnitAttentionTracker::new();

        // Initialize session with 3 LUNs
        tracker.initialize_session(1, 3);

        // All 3 LUNs should have power-on UA
        assert_eq!(
            tracker.check_and_pop_ua(1, 0),
            Some(UnitAttentionCode::POWER_ON_RESET)
        );
        assert_eq!(
            tracker.check_and_pop_ua(1, 1),
            Some(UnitAttentionCode::POWER_ON_RESET)
        );
        assert_eq!(
            tracker.check_and_pop_ua(1, 2),
            Some(UnitAttentionCode::POWER_ON_RESET)
        );
    }
}
