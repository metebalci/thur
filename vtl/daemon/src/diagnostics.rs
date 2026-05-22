// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! SCSI SEND DIAGNOSTIC / RECEIVE DIAGNOSTIC RESULTS support.
//!
//! Two LUN-routed self-tests, run only when the host sets the
//! `SELFTEST` bit in the SEND DIAGNOSTIC CDB. Anything else returns
//! GOOD without recording — same surface a real LTO drive presents
//! when the host issues a default no-test diagnostic.
//!
//! - **LU0 (medium changer)**: parse `library.json` + `inventory.json`,
//!   confirm every barcode in inventory has a readable
//!   `tapes/<barcode>/manifest.json`, and run the full
//!   `validate_cloud_backend` probe (auth + write + delete) against
//!   every named entry in `cloud.backends`. Mirrors the operator-side
//!   `thurvtl system cloud check`.
//! - **LU1+ (sequential-access drive)**: if a cartridge is loaded,
//!   re-read its `manifest.json` and confirm it parses; if no cartridge
//!   is loaded, the test is a trivial pass.
//!
//! Results land in a per-LUN ring buffer of the most-recent 20
//! entries (`DiagnosticStore`). The store, the ring depth, the
//! per-entry shape, and the SPC-4 page builders all live in
//! `scsi-ssc::diagnostics`. The library-vs-drive split below
//! (`run_library_diagnostic`, the `run_and_record` orchestrator) stays
//! library-local because LU0 self-test walks `library.json` /
//! `inventory.json`.

pub use scsi_ssc::diagnostics::{DiagnosticEntry, DiagnosticStore, run_drive_diagnostic};

use std::path::Path;
use std::sync::Arc;

use core_mediachanger::{CloudConfig, validate_cloud_backend};
use scsi_ssc::drive_manager::DriveManager;

/// LU0 self-test. Walks `<data_dir>/library/library.json` +
/// `inventory.json`, confirms every barcode in inventory has a
/// readable `manifest.json`, and runs the full
/// `validate_cloud_backend` probe against every named cloud backend.
/// Returns the first failure as a `DiagnosticEntry::fail` so
/// SEND DIAGNOSTIC can surface CHECK CONDITION immediately.
pub async fn run_library_diagnostic(
    data_dir: &Path,
    cloud_config: &CloudConfig,
) -> DiagnosticEntry {
    let library_dir = data_dir.join("library");
    let library_json = library_dir.join("library.json");
    let inventory_json = library_dir.join("inventory.json");

    let lib_text = match tokio::fs::read_to_string(&library_json).await {
        Ok(s) => s,
        Err(e) => {
            return DiagnosticEntry::fail(format!(
                "library.json unreadable ({}): {}",
                library_json.display(),
                e
            ));
        }
    };
    if let Err(e) = serde_json::from_str::<serde_json::Value>(&lib_text) {
        return DiagnosticEntry::fail(format!("library.json parse failed: {}", e));
    }

    let inv_text = match tokio::fs::read_to_string(&inventory_json).await {
        Ok(s) => s,
        Err(e) => {
            return DiagnosticEntry::fail(format!(
                "inventory.json unreadable ({}): {}",
                inventory_json.display(),
                e
            ));
        }
    };
    let inv_v: serde_json::Value = match serde_json::from_str(&inv_text) {
        Ok(v) => v,
        Err(e) => {
            return DiagnosticEntry::fail(format!("inventory.json parse failed: {}", e));
        }
    };

    let tapes_dir = data_dir.join("tapes");
    for key in ["storage_slots", "mail_slots", "drives"] {
        if let Some(arr) = inv_v.get(key).and_then(|x| x.as_array()) {
            for item in arr {
                let Some(barcode) = item.get("barcode").and_then(|s| s.as_str()) else {
                    continue;
                };
                if barcode.is_empty() {
                    continue;
                }
                let manifest = tapes_dir.join(barcode).join("manifest.json");
                let text = match tokio::fs::read_to_string(&manifest).await {
                    Ok(s) => s,
                    Err(e) => {
                        return DiagnosticEntry::fail(format!(
                            "cartridge '{}' manifest.json unreadable: {}",
                            barcode, e
                        ));
                    }
                };
                if let Err(e) = serde_json::from_str::<serde_json::Value>(&text) {
                    return DiagnosticEntry::fail(format!(
                        "cartridge '{}' manifest.json parse failed: {}",
                        barcode, e
                    ));
                }
            }
        }
    }

    for name in cloud_config.backend_names() {
        if let Err(e) = validate_cloud_backend(cloud_config, &name, |_| {}).await {
            return DiagnosticEntry::fail(format!("cloud backend '{}': {}", name, e));
        }
    }

    DiagnosticEntry::pass()
}

/// Entry point for the iSCSI request-loop pre-hook. Runs the
/// LUN-appropriate diagnostic and stamps the result into `store`.
/// The (sync) SEND DIAGNOSTIC handler later consults `store.last()`
/// to decide GOOD vs CHECK CONDITION; RECEIVE DIAGNOSTIC RESULTS
/// page 0x10 walks `store.snapshot()`.
pub async fn run_and_record(
    lun: u8,
    drive_manager: &Arc<DriveManager>,
    cloud_config: &Arc<CloudConfig>,
    data_dir: &Path,
    store: &Arc<DiagnosticStore>,
) {
    let entry = if lun == 0 {
        run_library_diagnostic(data_dir, cloud_config).await
    } else {
        let drive_id = (lun - 1) as usize;
        let dm = Arc::clone(drive_manager);
        let tapes_root = data_dir.join("tapes");
        match tokio::task::spawn_blocking(move || run_drive_diagnostic(&dm, drive_id, &tapes_root))
            .await
        {
            Ok(e) => e,
            Err(join_err) => {
                DiagnosticEntry::fail(format!("drive diagnostic task panicked: {}", join_err))
            }
        }
    };

    if entry.passed {
        tracing::info!("SEND DIAGNOSTIC self-test PASSED (LUN {})", lun);
    } else {
        tracing::warn!(
            "SEND DIAGNOSTIC self-test FAILED (LUN {}): {}",
            lun,
            entry.detail
        );
    }

    store.record(lun, entry);
}
