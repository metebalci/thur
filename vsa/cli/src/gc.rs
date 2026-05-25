// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `thurvsa system gc` — daemon-routed orphan-chunk garbage
//! collection.
//!
//! The daemon owns the chunk pool and audit log; the CLI is a thin
//! streaming client. With page-index mutations and pool sweeps
//! serialized through the daemon's locks, GC runs alongside live
//! iSCSI / NVMe-TCP traffic — no daemon-down gate.

use anyhow::{Context, Result};

use shared_admin_client::AdminClient;
use shared_admin_proto::JobEvent;

pub async fn cmd_gc(dry_run: bool, storage: bool) -> Result<()> {
    let client = AdminClient::auto_discover(&shared_naming::DISK);
    let body = serde_json::json!({
        "dry_run": dry_run,
        "storage": storage,
    });

    let exit = client
        .run_job("system.gc", &body, |ev| match ev {
            JobEvent::Log { level, message } => {
                if level == "warn" || level == "error" {
                    eprintln!("{}", message);
                } else {
                    println!("{}", message);
                }
            }
            // Result is captured for completeness but the streamed
            // log lines already give the operator everything they
            // need.
            JobEvent::Result { .. } | JobEvent::Progress { .. } | JobEvent::Done { .. } => {}
        })
        .await
        .context("gc job stream")?;

    if exit != 0 {
        std::process::exit(exit);
    }
    Ok(())
}
