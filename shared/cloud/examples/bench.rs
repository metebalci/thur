// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0
//
// First-party cloud-backend throughput benchmark — dev wrapper.
//
// Operators on a packaged install should prefer the daemon-down CLI
// verbs `thurvtl system cloud benchmark` / `thurvsa system
// cloud benchmark`, which parse the daemon YAML conffile and call the
// same engine via `shared-cloud-bench`. This example exists so
// developers can drive the engine against an ad-hoc YAML file without
// building either CLI binary.
//
// Usage:
//   cargo run --release -p shared-cloud --example bench -- \
//     --config <PATH> [--backend NAME]... \
//     [--total-gb N] [--chunk-size-mb N] [--concurrency N] \
//     [--chunk-size-mb-sweep N,N,N] [--concurrency-sweep N,N,N] \
//     [--skip-download] [--yes]

use clap::Parser;
use shared_cloud::CloudConfig;
use shared_cloud_bench::{BenchOptions, BenchTarget};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(about = "First-party cloud-backend throughput benchmark (dev)")]
struct Args {
    /// Path to a daemon YAML conffile carrying a `cloud.backends:`
    /// block. Required.
    #[arg(long)]
    config: PathBuf,

    /// Backend name to benchmark. Repeatable. Defaults to every
    /// backend defined under `cloud.backends:` (deterministic order).
    #[arg(long = "backend")]
    backends: Vec<String>,

    /// GiB per cell. Default 32 — long enough for TCP steady-state on
    /// every measured backend.
    #[arg(long, default_value_t = 32)]
    total_gb: usize,

    /// MiB per upload. Default 8 matches the FastCDC chunk average.
    /// Mutually exclusive with --chunk-size-mb-sweep.
    #[arg(long, default_value_t = 8)]
    chunk_size_mb: usize,

    /// Parallel in-flight uploads per cell. Default 16 picks a
    /// ceiling-finding value (daemon's runtime default is 8).
    /// Mutually exclusive with --concurrency-sweep.
    #[arg(long, default_value_t = 16)]
    concurrency: usize,

    /// Sweep chunk size across this comma-separated list.
    #[arg(long, value_delimiter = ',')]
    chunk_size_mb_sweep: Vec<usize>,

    /// Sweep concurrency across this comma-separated list.
    #[arg(long, value_delimiter = ',')]
    concurrency_sweep: Vec<usize>,

    /// Skip the download phase.
    #[arg(long)]
    skip_download: bool,

    /// Bypass the sweep-preview prompt (scripted runs).
    #[arg(long)]
    yes: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    #[derive(serde::Deserialize)]
    struct CloudOnly {
        #[serde(default)]
        cloud: CloudConfig,
    }
    let body = std::fs::read_to_string(&args.config)?;
    let parsed: CloudOnly = serde_yaml::from_str(&body)?;
    let cfg = parsed.cloud;

    let backend_names: Vec<String> = if args.backends.is_empty() {
        cfg.backends.keys().cloned().collect()
    } else {
        for name in &args.backends {
            if !cfg.backends.contains_key(name) {
                return Err(format!("backend '{name}' not in {:?}", args.config).into());
            }
        }
        args.backends.clone()
    };
    if backend_names.is_empty() {
        return Err(format!(
            "no backends defined under `cloud.backends:` in {:?}",
            args.config
        )
        .into());
    }

    let mut targets = Vec::with_capacity(backend_names.len());
    for name in &backend_names {
        eprintln!("constructing backend '{}'…", name);
        let backend = cfg.create_backend_named(name).await?;
        targets.push(BenchTarget {
            name: name.clone(),
            backend,
        });
    }

    let opts = BenchOptions {
        total_gb: args.total_gb,
        chunk_size_mb: args.chunk_size_mb,
        concurrency: args.concurrency,
        chunk_size_mb_sweep: args.chunk_size_mb_sweep,
        concurrency_sweep: args.concurrency_sweep,
        skip_download: args.skip_download,
        yes: args.yes,
    };

    shared_cloud_bench::run(targets, opts).await?;
    Ok(())
}
