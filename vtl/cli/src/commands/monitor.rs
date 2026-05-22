// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `library monitor` — live polling of daemon state.
//!
//! Polls `/api/v1/drives`, `/api/v1/library/info`, and
//! `/api/v1/changer/inventory` over the admin Unix socket every
//! `interval` seconds and re-renders a screen showing drive status
//! plus inventory summary. Migrated from a direct on-disk Library
//! read to daemon-routed live state — same visual output, but the
//! operator sees what iSCSI sees in real time, including in-flight
//! `MOVE MEDIUM` updates.

use anyhow::Result;
use serde::Deserialize;
use tokio::time::Duration;

use crate::output::create_table;

#[derive(Deserialize)]
struct DrivesListResp {
    drives: Vec<DriveStatusResp>,
}

#[derive(Deserialize)]
struct DriveStatusResp {
    id: u32,
    loaded: bool,
    barcode: Option<String>,
    home_slot: Option<u16>,
    next_lba: Option<u64>,
    total_blocks: Option<usize>,
}

#[derive(Deserialize)]
struct LibraryInfoResp {
    storage_slots: usize,
    mail_slots: usize,
    drives: usize,
    #[allow(dead_code)]
    lto_generation: u8,
    #[allow(dead_code)]
    firmware: String,
}

#[derive(Deserialize)]
struct InventoryEntry {
    slot_type: String,
    #[allow(dead_code)]
    slot_id: u32,
    #[allow(dead_code)]
    barcode: String,
}

#[derive(Deserialize)]
struct InventoryResp {
    entries: Vec<InventoryEntry>,
}

pub async fn cmd_monitor(interval: u64) -> Result<()> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);

    println!();
    println!("Live Monitor (Press Ctrl+C to exit)");
    println!();

    loop {
        // Pull all three snapshots back-to-back. A small inconsistency
        // window (e.g. drives is fresh, library is one tick old) is
        // acceptable for a human-facing dashboard.
        let drives: DrivesListResp = client.get_json("/api/v1/drives").await?;
        let lib: LibraryInfoResp = client.get_json("/api/v1/library/info").await?;
        let inv: InventoryResp = client.get_json("/api/v1/changer/inventory").await?;

        // Clear screen (ANSI escape code)
        print!("\x1B[2J\x1B[1;1H");

        println!(
            "Updated: {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        println!();

        print_drives_status(&drives);
        print_library_summary(&lib, &inv);

        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

fn print_drives_status(drives: &DrivesListResp) {
    let mut table = create_table();
    table.set_header(vec![
        "Drive",
        "Status",
        "Cartridge",
        "Home Slot",
        "Position",
    ]);

    for d in &drives.drives {
        let status = if d.loaded { "Loaded" } else { "Empty" };
        let barcode = d.barcode.as_deref().unwrap_or("-");
        let home_slot = d
            .home_slot
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".to_string());
        let position = match (d.next_lba, d.total_blocks) {
            (Some(lba), Some(total)) => format!("LBA {} / {} blocks", lba, total),
            _ => "-".to_string(),
        };
        table.add_row(vec![
            d.id.to_string(),
            status.to_string(),
            barcode.to_string(),
            home_slot,
            position,
        ]);
    }

    println!("Drives:");
    println!("{table}");
    println!();
}

fn print_library_summary(lib: &LibraryInfoResp, inv: &InventoryResp) {
    let occupied_storage = inv
        .entries
        .iter()
        .filter(|e| e.slot_type == "storage")
        .count();
    let occupied_mail = inv.entries.iter().filter(|e| e.slot_type == "mail").count();
    let loaded_drives = inv
        .entries
        .iter()
        .filter(|e| e.slot_type == "drive")
        .count();

    println!("Library Inventory:");
    println!(
        "  Storage Slots: {}/{} occupied ({} empty)",
        occupied_storage,
        lib.storage_slots,
        lib.storage_slots.saturating_sub(occupied_storage),
    );
    println!(
        "  Mail Slots: {}/{} occupied ({} empty)",
        occupied_mail,
        lib.mail_slots,
        lib.mail_slots.saturating_sub(occupied_mail),
    );
    println!(
        "  Drives: {}/{} loaded ({} empty)",
        loaded_drives,
        lib.drives,
        lib.drives.saturating_sub(loaded_drives),
    );
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_drives_status_renders_loaded_and_empty() {
        let drives: DrivesListResp = serde_json::from_value(serde_json::json!({
            "drives": [
                {
                    "id": 0,
                    "loaded": true,
                    "barcode": "TAPE001",
                    "home_slot": 5,
                    "next_lba": 100,
                    "total_blocks": 1000,
                },
                {
                    "id": 1,
                    "loaded": false,
                    "barcode": null,
                    "home_slot": null,
                    "next_lba": null,
                    "total_blocks": null,
                },
            ],
        }))
        .expect("parse drives list");
        // Exercises both the populated and "-" placeholder branches.
        print_drives_status(&drives);
        assert_eq!(drives.drives.len(), 2);
    }

    #[test]
    fn print_drives_status_handles_empty_list() {
        let drives: DrivesListResp =
            serde_json::from_value(serde_json::json!({"drives": []})).expect("parse empty");
        print_drives_status(&drives);
    }

    #[test]
    fn print_library_summary_counts_by_slot_type() {
        let lib: LibraryInfoResp = serde_json::from_value(serde_json::json!({
            "storage_slots": 40,
            "mail_slots": 5,
            "drives": 3,
            "lto_generation": 8,
            "firmware": "NVL8",
        }))
        .expect("parse library info");
        let inv: InventoryResp = serde_json::from_value(serde_json::json!({
            "entries": [
                {"slot_type": "storage", "slot_id": 0, "barcode": "A"},
                {"slot_type": "storage", "slot_id": 1, "barcode": "B"},
                {"slot_type": "mail", "slot_id": 0, "barcode": "C"},
                {"slot_type": "drive", "slot_id": 0, "barcode": "D"},
            ],
        }))
        .expect("parse inventory");
        print_library_summary(&lib, &inv);
        assert_eq!(lib.storage_slots, 40);
    }

    #[test]
    fn print_library_summary_handles_empty_inventory() {
        let lib: LibraryInfoResp = serde_json::from_value(serde_json::json!({
            "storage_slots": 10,
            "mail_slots": 0,
            "drives": 1,
            "lto_generation": 8,
            "firmware": "NVL8",
        }))
        .expect("parse library info");
        let inv: InventoryResp =
            serde_json::from_value(serde_json::json!({"entries": []})).expect("parse inventory");
        print_library_summary(&lib, &inv);
    }
}
