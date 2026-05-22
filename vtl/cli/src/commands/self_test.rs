// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `thurvtl {library,drive} self-test` — daemon-routed SPC-4
//! self-tests.
//!
//! Same diagnostic the iSCSI SEND DIAGNOSTIC handler runs; routing
//! through the admin socket lets operators trigger a probe without
//! an initiator on the host. The result is also stamped into the
//! daemon's per-LUN ring buffer so a subsequent host RECEIVE
//! DIAGNOSTIC RESULTS sees it as the latest entry.

use anyhow::{Context, Result};

use shared_admin_client::AdminClient;
use shared_admin_proto::JobEvent;

pub async fn cmd_library_self_test(json: bool) -> Result<i32> {
    run("system.library.self_test", &serde_json::json!({}), json).await
}

pub async fn cmd_drive_self_test(drive: u16, json: bool) -> Result<i32> {
    run(
        "system.drive.self_test",
        &serde_json::json!({"drive": drive as u32}),
        json,
    )
    .await
}

async fn run(kind: &str, body: &serde_json::Value, json: bool) -> Result<i32> {
    let client = AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let mut result_payload: Option<serde_json::Value> = None;

    let exit = client
        .run_job(kind, body, |ev| match ev {
            JobEvent::Log { level, message } => {
                if level == "warn" || level == "error" {
                    eprintln!("{}", message);
                } else if !json {
                    println!("{}", message);
                } else {
                    eprintln!("{}", message);
                }
            }
            JobEvent::Result { data } => {
                result_payload = Some(data);
            }
            JobEvent::Progress { .. } | JobEvent::Done { .. } => {}
        })
        .await
        .with_context(|| format!("{} stream", kind))?;

    if json && let Some(v) = result_payload {
        println!("{}", serde_json::to_string_pretty(&v)?);
    }
    Ok(exit)
}
