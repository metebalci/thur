// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `<product> system daemon-health` — probe the admin Unix socket.
//!
//! Tiny smoke command that proves the CLI ↔ daemon Unix-socket
//! transport is wired end-to-end: open an `AdminClient`, call `GET
//! /api/v1/health`, and render the daemon's identity. Generic over
//! `ProductIdentity` so `thurvtl` and `thurvsa` share one implementation.

use anyhow::Result;
use serde::Deserialize;
use shared_admin_client::AdminClient;
use shared_naming::ProductIdentity;

#[derive(Debug, Deserialize)]
struct HealthResponse {
    status: String,
    daemon: String,
    version: String,
    data_dir: String,
    api_version: String,
}

/// `system daemon-health`. Connects to the product's admin socket,
/// calls `GET /api/v1/health`, and renders the daemon identity.
pub async fn cmd_daemon_health(product: &'static ProductIdentity, json: bool) -> Result<()> {
    let client = AdminClient::auto_discover(product);
    let resp: HealthResponse = client.get_json("/api/v1/health").await?;

    if json {
        // Re-serialize so the output is canonical (sorted keys would
        // be nicer but serde_json doesn't promise that — pretty is
        // enough for human-readable jq).
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": resp.status,
                "daemon": resp.daemon,
                "version": resp.version,
                "data_dir": resp.data_dir,
                "api_version": resp.api_version,
            }))?
        );
        return Ok(());
    }

    println!("Daemon:      {}", resp.daemon);
    println!("Version:     {}", resp.version);
    println!("Status:      {}", resp.status);
    println!("API:         {}", resp.api_version);
    println!("Data dir:    {}", resp.data_dir);
    println!("Socket:      {}", client.socket_path().display());
    Ok(())
}
