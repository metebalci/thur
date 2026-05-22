// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! SCSI SEND DIAGNOSTIC / RECEIVE DIAGNOSTIC RESULTS state shared by
//! both tape products.
//!
//! [`DiagnosticStore`] is a per-LUN ring buffer of the most-recent 20
//! self-test results. The shared SEND DIAGNOSTIC handler reads
//! `store.last(lun)` to decide GOOD vs CHECK CONDITION; the shared
//! RECEIVE DIAGNOSTIC RESULTS handler walks `store.snapshot(lun)` to
//! emit the SPC-4 §7.2.21 Self-Test Results page.
//!
//! Test runners that populate the store stay library-local: thurvtl
//! drives a library-vs-drive split (`run_library_diagnostic` checks
//! `library.json` + `inventory.json` + every cartridge manifest +
//! every cloud backend). The drive-tier runner [`run_drive_diagnostic`]
//! is shared because it only reads the loaded cartridge's
//! `manifest.json`.

use chrono::{DateTime, Utc};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Mutex;

use crate::drive_manager::DriveManager;

/// Ring depth — matches SPC-4 §7.2.21 Self-Test Results which carries
/// at most 20 entries (page length 20×20 = 0x0190).
pub const RING_DEPTH: usize = 20;

/// One self-test invocation. `passed` controls SEND DIAGNOSTIC's
/// terminal status (GOOD vs CHECK CONDITION); `sense_key`/`asc`/`ascq`
/// are reflected back in the Self-Test Results page so the host can
/// see which component failed without having to issue REQUEST SENSE
/// again.
#[derive(Debug, Clone)]
pub struct DiagnosticEntry {
    pub timestamp: DateTime<Utc>,
    pub passed: bool,
    pub sense_key: u8,
    pub asc: u8,
    pub ascq: u8,
    /// One-line operator-facing description of the failing component.
    /// Logged via `tracing` and the audit subsystem; not part of the
    /// SCSI surface (page 0x10 carries only the sense triple).
    pub detail: String,
}

impl DiagnosticEntry {
    pub fn pass() -> Self {
        Self {
            timestamp: Utc::now(),
            passed: true,
            sense_key: 0,
            asc: 0,
            ascq: 0,
            detail: String::new(),
        }
    }

    /// HARDWARE ERROR (0x04) / DIAGNOSTIC FAILURE ON COMPONENT 80h
    /// (0x40/0x80). Generic self-test failure marker — `detail`
    /// distinguishes which check tripped for the operator.
    pub fn fail(detail: impl Into<String>) -> Self {
        Self {
            timestamp: Utc::now(),
            passed: false,
            sense_key: 0x04,
            asc: 0x40,
            ascq: 0x80,
            detail: detail.into(),
        }
    }
}

/// Per-LUN ring buffer of `DiagnosticEntry` values. Volatile —
/// SPC-4 doesn't require Self-Test Results to survive power-cycle
/// (real drives keep them in NVRAM as a courtesy, but the host can
/// always re-issue SEND DIAGNOSTIC).
pub struct DiagnosticStore {
    rings: Mutex<HashMap<u8, VecDeque<DiagnosticEntry>>>,
}

impl DiagnosticStore {
    pub fn new() -> Self {
        Self {
            rings: Mutex::new(HashMap::new()),
        }
    }

    /// Push a new result at the head; evict the oldest once the ring
    /// hits `RING_DEPTH`.
    pub fn record(&self, lun: u8, entry: DiagnosticEntry) {
        let mut rings = match self.rings.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(), // poisoned: treat as empty, recover
        };
        let ring = rings.entry(lun).or_default();
        if ring.len() == RING_DEPTH {
            ring.pop_back();
        }
        ring.push_front(entry);
    }

    /// Most recent entry for `lun`, or `None` if SEND DIAGNOSTIC has
    /// never been issued against this LUN.
    pub fn last(&self, lun: u8) -> Option<DiagnosticEntry> {
        let rings = match self.rings.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        rings.get(&lun).and_then(|r| r.front()).cloned()
    }

    /// Snapshot of the ring (most-recent first, up to `RING_DEPTH`).
    pub fn snapshot(&self, lun: u8) -> Vec<DiagnosticEntry> {
        let rings = match self.rings.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        rings
            .get(&lun)
            .map(|r| r.iter().cloned().collect())
            .unwrap_or_default()
    }
}

impl Default for DiagnosticStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Drive-tier self-test. With a cartridge loaded, re-reads the on-disk
/// `manifest.json` to confirm it still parses; without a cartridge,
/// always passes (drive responding is the only thing the host can
/// observe). Sync because every check is a local file read under the
/// per-drive Mutex; SEND DIAGNOSTIC is not a hot path.
pub fn run_drive_diagnostic(
    drive_manager: &DriveManager,
    drive_id: usize,
    tapes_root: &Path,
) -> DiagnosticEntry {
    let label = match drive_manager.get_cartridge_label(drive_id) {
        Ok(Some(l)) => l,
        Ok(None) => return DiagnosticEntry::pass(),
        Err(e) => {
            return DiagnosticEntry::fail(format!("drive {} unreachable: {}", drive_id, e));
        }
    };

    let manifest = tapes_root.join(&label).join("manifest.json");
    let text = match std::fs::read_to_string(&manifest) {
        Ok(s) => s,
        Err(e) => {
            return DiagnosticEntry::fail(format!(
                "cartridge '{}' manifest.json unreadable: {}",
                label, e
            ));
        }
    };
    if let Err(e) = serde_json::from_str::<serde_json::Value>(&text) {
        return DiagnosticEntry::fail(format!(
            "cartridge '{}' manifest.json parse failed: {}",
            label, e
        ));
    }

    DiagnosticEntry::pass()
}

/// Build the SPC-4 §7.2.21 Self-Test Results page (page code 0x10)
/// for the given LUN. The page is always 4 + 20×20 = 404 bytes long
/// (page length field = 0x0190); unused entries are zero-filled.
/// Most recent result occupies the first parameter slot.
pub fn build_self_test_results_page(store: &DiagnosticStore, lun: u8) -> Vec<u8> {
    let entries = store.snapshot(lun);
    let mut out = Vec::with_capacity(4 + RING_DEPTH * 20);

    // Page header (4 bytes).
    out.push(0x10); // page code
    out.push(0x00); // reserved
    out.push(0x01); // page length MSB (= 0x0190 = 400)
    out.push(0x90); // page length LSB

    for slot in 0..RING_DEPTH {
        // Parameter header (4 bytes) — parameter code is the
        // sequence number 0x0001..0x0014; control byte uses
        // FORMAT=11b (bounded) + LP=1 (binary list parameter) =
        // 0x03; parameter length = 16.
        out.push(0x00);
        out.push((slot + 1) as u8);
        out.push(0x03);
        out.push(0x10);

        // Parameter data (16 bytes).
        let mut p = [0u8; 16];
        if let Some(e) = entries.get(slot) {
            // Byte 0: SELF-TEST CODE (host-issued, default 0) <<4 |
            // SELF-TEST RESULTS VALUE (0=pass, 3=unknown_error,
            // 7=test failed segment unknown).
            let result_value: u8 = if e.passed { 0x00 } else { 0x07 };
            p[0] = result_value & 0x0F;
            p[1] = 0; // SELF-TEST NUMBER (vendor)
            // Bytes 2-3: ACCUMULATED POWER ON HOURS (0 — virtual drive)
            // Bytes 4-11: ADDRESS OF FIRST FAILURE (0 — N/A on a self-test that doesn't probe LBAs)
            // Byte 12: bits 3..0 = SENSE KEY
            p[12] = e.sense_key & 0x0F;
            p[13] = e.asc;
            p[14] = e.ascq;
            // Byte 15: VENDOR SPECIFIC (= 0)
        }
        out.extend_from_slice(&p);
    }

    out
}

/// SPC-4 §6.21 Supported Diagnostic Pages (page 0x00). Lists every
/// page the device server supports — `[0x00, 0x10]`.
pub fn build_supported_diagnostic_pages() -> Vec<u8> {
    let codes: [u8; 2] = [0x00, 0x10];
    let mut out = Vec::with_capacity(4 + codes.len());
    out.push(0x00); // page code
    out.push(0x00); // reserved
    out.push(0x00); // page length MSB
    out.push(codes.len() as u8); // page length LSB
    out.extend_from_slice(&codes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_evicts_oldest_at_depth_cap() {
        let store = DiagnosticStore::new();
        for i in 0..(RING_DEPTH + 5) {
            let mut e = DiagnosticEntry::pass();
            e.detail = format!("entry-{}", i);
            store.record(0, e);
        }
        let snap = store.snapshot(0);
        assert_eq!(snap.len(), RING_DEPTH);
        // Most-recent first: top entry is the last one we pushed.
        assert_eq!(snap[0].detail, format!("entry-{}", RING_DEPTH + 4));
        // Oldest surviving entry is index `5` (entries 0..4 evicted).
        assert_eq!(snap[RING_DEPTH - 1].detail, "entry-5");
    }

    #[test]
    fn supported_pages_lists_self_test_results() {
        let data = build_supported_diagnostic_pages();
        assert_eq!(data[0], 0x00);
        let len = u16::from_be_bytes([data[2], data[3]]) as usize;
        assert_eq!(len, data.len() - 4);
        assert!(data[4..].contains(&0x10));
    }

    #[test]
    fn self_test_results_page_layout() {
        let store = DiagnosticStore::new();
        store.record(0, DiagnosticEntry::pass());
        store.record(0, DiagnosticEntry::fail("simulated cloud auth fail"));

        let data = build_self_test_results_page(&store, 0);
        // Header
        assert_eq!(data[0], 0x10);
        assert_eq!(u16::from_be_bytes([data[2], data[3]]), 0x0190);
        // Total = header + 20 entries × 20 bytes
        assert_eq!(data.len(), 4 + RING_DEPTH * 20);

        // First parameter (most recent — the failure we just pushed).
        let p0 = &data[4..24];
        assert_eq!(u16::from_be_bytes([p0[0], p0[1]]), 0x0001); // parameter code
        assert_eq!(p0[2], 0x03); // control byte (FORMAT=11b, LP=1)
        assert_eq!(p0[3], 0x10); // parameter length
        // result_value=7 (test failed - segment unknown) for failures.
        assert_eq!(p0[4] & 0x0F, 0x07);
        assert_eq!(p0[16], 0x04); // HARDWARE ERROR sense key
        assert_eq!(p0[17], 0x40); // ASC = DIAGNOSTIC FAILURE ON COMPONENT
        assert_eq!(p0[18], 0x80);

        // Second parameter (older — the pass).
        let p1 = &data[24..44];
        assert_eq!(u16::from_be_bytes([p1[0], p1[1]]), 0x0002);
        assert_eq!(p1[4] & 0x0F, 0x00); // result_value=0 (pass)
        assert_eq!(p1[16], 0x00); // sense key zero on pass
    }

    #[test]
    fn empty_lun_renders_all_zero_entries() {
        let store = DiagnosticStore::new();
        let data = build_self_test_results_page(&store, 7);
        assert_eq!(data.len(), 4 + RING_DEPTH * 20);
        // Body bytes 4.. should all be zero except the parameter
        // headers (code + control + length on every slot).
        for slot in 0..RING_DEPTH {
            let off = 4 + slot * 20;
            assert_eq!(
                u16::from_be_bytes([data[off], data[off + 1]]) as usize,
                slot + 1
            );
            assert_eq!(data[off + 2], 0x03);
            assert_eq!(data[off + 3], 0x10);
            for b in &data[off + 4..off + 20] {
                assert_eq!(*b, 0);
            }
        }
    }
}
