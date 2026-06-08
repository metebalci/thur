// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `thurvtl system storage benchmark` — daemon-down throughput
//! benchmark.
//!
//! Parses the YAML conffile's `storage.backends:`, constructs each
//! backend, and hands them to `shared-object-store-bench`. Doesn't
//! touch the admin socket so it works pre-daemon-start.
//!
//! The sibling `system storage check` verb (daemon-routed storage
//! reachability probe) lives in `shared-cli-system` so VTL and VSA
//! share one implementation.

use anyhow::Result;
use shared_object_store_bench::BenchOptions;
use std::path::Path;

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
    shared_object_store_bench::run_from_config_path(Path::new(config_path), backends, opts)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
