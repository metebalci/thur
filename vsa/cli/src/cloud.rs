// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `thurvsa system cloud benchmark` — daemon-down first-party
//! throughput benchmark. Parses the daemon YAML conffile, constructs
//! each named backend, hands the lot to `shared-cloud-bench`. Doesn't
//! touch the admin socket so operators can validate a freshly-
//! configured backend before the daemon ever opens it.

use anyhow::Result;
use shared_cloud_bench::BenchOptions;
use std::path::Path;

#[allow(clippy::too_many_arguments)]
pub async fn cmd_benchmark(
    config_path: &Path,
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
    shared_cloud_bench::run_from_config_path(config_path, backends, opts)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
