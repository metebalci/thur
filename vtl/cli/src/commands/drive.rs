// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
struct DriveStatusResponse {
    id: u32,
    loaded: bool,
    barcode: Option<String>,
    home_slot: Option<u16>,
    next_lba: Option<u64>,
    total_blocks: Option<usize>,
}

pub async fn cmd_status(drive: u16, json: bool) -> Result<()> {
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let path = format!("/api/v1/drives/{}", drive);
    let status: DriveStatusResponse = client.get_json(&path).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }

    println!();
    println!("Drive {} Status", drive);
    println!("---------------");
    println!("Status: {}", if status.loaded { "Loaded" } else { "Empty" });

    if let Some(ref barcode) = status.barcode {
        println!("Cartridge: {}", barcode);
    }
    if let Some(source) = status.home_slot {
        println!("Home slot: {}", source);
    }
    if status.loaded
        && let (Some(next_lba), Some(blocks)) = (status.next_lba, status.total_blocks)
    {
        println!();
        println!("Tape Position:");
        println!("  Next LBA: {}", next_lba);
        println!("  Total blocks: {}", blocks);
    }
    Ok(())
}

#[derive(Serialize)]
struct DriveResetStatsBody {
    drive: Option<u32>,
    all: bool,
}

#[derive(Deserialize)]
struct DriveResetStatsResp {
    affected_drives: usize,
}

pub async fn cmd_reset_stats(drive: Option<u16>, all: bool) -> Result<()> {
    if all && drive.is_some() {
        anyhow::bail!("pass a drive id or --all, not both");
    }
    if !all && drive.is_none() {
        anyhow::bail!("specify a drive id or --all");
    }
    let client = shared_admin_client::AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let body = DriveResetStatsBody {
        drive: drive.map(u32::from),
        all,
    };
    let resp: DriveResetStatsResp = client
        .post_json("/api/v1/drives/reset-stats", &body)
        .await?;
    match drive {
        Some(id) => println!("OK: reset stats for drive {}", id),
        None => println!("OK: reset stats for all {} drive(s)", resp.affected_drives),
    }
    Ok(())
}
