// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `thurvtl system cloud check` — daemon-routed cloud
//! reachability probe.
//!
//! The daemon already holds a parsed `cloud.backends` config; routing
//! the check through the admin socket lets it reuse those handles
//! without re-parsing YAML CLI-side, and lands the same audit trail
//! every other live op produces. The CLI streams the daemon's NDJSON
//! event log and exits with the daemon's reported code.
//!
//! `thurvtl system cloud benchmark` is daemon-down: parses the
//! YAML conffile's `cloud.backends:`, constructs each backend, hands
//! them to `shared-cloud-bench`. Doesn't touch the admin socket so it
//! works pre-daemon-start.

use anyhow::Result;
use shared_admin_client::AdminClient;
use shared_admin_proto::JobEvent;
use shared_cloud_bench::BenchOptions;
use std::path::Path;

pub async fn cmd_check() -> Result<()> {
    let client = AdminClient::auto_discover(&shared_naming::TAPE_LIBRARY);
    let exit = client
        .run_job("system.cloud_check", &serde_json::json!({}), |ev| {
            render_event(ev);
        })
        .await?;
    if exit != 0 {
        std::process::exit(exit);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_benchmark(
    config_path: &str,
    backends: Vec<String>,
    total_gb: usize,
    chunk_size_mb: usize,
    concurrency: usize,
    chunk_size_mb_sweep: Vec<usize>,
    concurrency_sweep: Vec<usize>,
    skip_download: bool,
    yes: bool,
) -> Result<()> {
    let opts = BenchOptions {
        total_gb,
        chunk_size_mb,
        concurrency,
        chunk_size_mb_sweep,
        concurrency_sweep,
        skip_download,
        yes,
    };
    shared_cloud_bench::run_from_config_path(Path::new(config_path), backends, opts)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

fn render_event(ev: JobEvent) {
    match ev {
        JobEvent::Log { level, message } => {
            // Match the prior CLI shape: pass-through to stdout for
            // info, stderr for warn/error. Daemon already formats the
            // [PASS]/[FAIL]/Diagnosis lines; we just relay them.
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
        // Result is the structured payload — useful for scripting via
        // `thurvtl ... --json`, but the human form here just
        // ignores it (the log lines already covered everything).
        JobEvent::Result { .. } => {}
        JobEvent::Done { .. } => {}
    }
}
