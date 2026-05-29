// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cross-product `system gc` — daemon-routed orphan-chunk garbage
//! collection.
//!
//! The daemon owns the chunk pool and audit log; the CLI is a thin
//! streaming client. With index mutations and pool sweeps serialized
//! through the daemon's locks, GC runs alongside live host traffic —
//! no daemon-down gate. Identical for VTL (`chunks.idx`) and VSA
//! (`pages.idx`); the only per-product input is the
//! [`ProductIdentity`] used for admin-socket discovery.

use anyhow::{Context, Result};

use shared_admin_client::AdminClient;
use shared_admin_proto::JobEvent;
use shared_naming::ProductIdentity;

pub async fn cmd_gc(
    identity: &'static ProductIdentity,
    dry_run: bool,
    storage: bool,
) -> Result<()> {
    let client = AdminClient::auto_discover(identity);
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
