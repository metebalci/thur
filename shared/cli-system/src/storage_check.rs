// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cross-product `system storage check` — daemon-routed storage
//! reachability probe.
//!
//! The daemon already holds a parsed `storage.backends` config; routing
//! the check through the admin socket lets it reuse those handles
//! without re-parsing YAML CLI-side, and lands the same audit trail
//! every other live op produces. The CLI streams the daemon's NDJSON
//! event log and exits with the daemon's reported code. Identical for
//! VTL and VSA; the only per-product input is the [`ProductIdentity`]
//! used for admin-socket discovery.

use anyhow::{Context, Result};

use shared_admin_client::AdminClient;
use shared_admin_proto::JobEvent;
use shared_naming::ProductIdentity;

pub async fn cmd_storage_check(identity: &'static ProductIdentity) -> Result<()> {
    let client = AdminClient::auto_discover(identity);
    let exit = client
        .run_job(
            "system.storage_check",
            &serde_json::json!({}),
            |ev| match ev {
                JobEvent::Log { level, message } => {
                    // The daemon already formats the [PASS]/[FAIL]/Diagnosis
                    // lines; relay info to stdout, warn/error to stderr.
                    if level == "warn" || level == "error" {
                        eprintln!("{}", message);
                    } else {
                        println!("{}", message);
                    }
                }
                JobEvent::Progress {
                    stage,
                    current,
                    total,
                } => match total {
                    Some(t) => println!("[progress] {} {}/{}", stage, current, t),
                    None => println!("[progress] {} {}", stage, current),
                },
                // The streamed log lines already cover everything; the
                // structured Result / terminal Done need no human render.
                JobEvent::Result { .. } | JobEvent::Done { .. } => {}
            },
        )
        .await
        .context("storage check job stream")?;
    if exit != 0 {
        std::process::exit(exit);
    }
    Ok(())
}
