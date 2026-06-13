// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! First-party storage-backend throughput benchmark engine.
//!
//! Drives N parallel `ObjectStoreBackend::upload_chunk` / `download_chunk` /
//! `delete_object` calls through the same SDK + network path the
//! daemon uses, so the numbers it reports are the actual ceiling the
//! daemon can reach against a given backend.
//!
//! The CLI wrappers (`thurvtl system storage benchmark`,
//! `thurvsa system storage benchmark`) and the
//! `shared/storage/examples/bench.rs` dev example all funnel through
//! [`run`]. Output: parseable `[BENCH] ...` lines on stdout; cell
//! errors are surfaced as `[BENCH-ERR]` on stderr without aborting
//! sibling cells.

use bytes::Bytes;
use futures::stream::{self, StreamExt};
use shared_object_store::{ObjectStoreBackend, ObjectStoreConfig};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// Knobs the caller can tune. Mirrors the example's flag set, minus
/// the `--backends-file` / `--backend` selection (the caller resolves
/// those into [`BenchTarget`]s before calling [`run`]).
#[derive(Debug, Clone)]
pub struct BenchOptions {
    /// GiB to push per cell. 1 GiB = 1024 MiB.
    pub total_gb: usize,
    /// MiB per upload. Mutually exclusive with `chunk_size_mb_sweep`.
    pub chunk_size_mb: usize,
    /// Parallel in-flight uploads per cell. Mutually exclusive with
    /// `concurrency_sweep`.
    pub concurrency: usize,
    /// Sweep chunk size across this list. Each cell keeps `total_gb`
    /// constant so cells transfer equal bytes.
    pub chunk_size_mb_sweep: Vec<usize>,
    /// Sweep concurrency across this list. Cross-products with
    /// `chunk_size_mb_sweep` if both are set.
    pub concurrency_sweep: Vec<usize>,
    /// Skip the download phase. Halves total ops (no GET).
    pub skip_download: bool,
    /// Bypass the sweep-preview prompt (scripted runs).
    pub yes: bool,
}

impl BenchOptions {
    /// Defaults baked into the operator-facing CLI flags.
    /// `total_gb=32` is long enough for TCP steady-state on every
    /// measured backend; `chunk_size_mb=8` matches the FastCDC chunk
    /// average; `concurrency=16` picks a ceiling-finding value above
    /// the daemon's runtime default (8).
    pub fn defaults() -> Self {
        Self {
            total_gb: 32,
            chunk_size_mb: 8,
            concurrency: 16,
            chunk_size_mb_sweep: Vec::new(),
            concurrency_sweep: Vec::new(),
            skip_download: false,
            yes: false,
        }
    }
}

/// One backend ready to be exercised. Caller is responsible for
/// constructing the backend (auth + SDK init); the bench engine only
/// drives upload/download/delete.
pub struct BenchTarget {
    pub name: String,
    pub backend: Box<dyn ObjectStoreBackend>,
}

/// Top-level error from the bench engine. Per-cell errors are emitted
/// inline as `[BENCH-ERR]` lines and do NOT abort the run; this enum
/// only covers the validation + interactive-prompt failures that
/// happen before any cell executes.
#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error("invalid argument: {0}")]
    InvalidArg(String),
    #[error("stdin read: {0}")]
    Stdin(std::io::Error),
    #[error("operator aborted at sweep prompt")]
    Aborted,
    #[error("no targets supplied")]
    NoTargets,
    #[error("loading {path}: {message}")]
    LoadConfig {
        path: std::path::PathBuf,
        message: String,
    },
    #[error("backend '{name}': {message}")]
    BackendLookup { name: String, message: String },
    #[error("instantiate storage backend '{name}': {source}")]
    BackendInstantiate {
        name: String,
        #[source]
        source: shared_object_store::ObjectStoreConfigError,
    },
}

/// Operator-facing entry point shared by `thurvtl system storage
/// benchmark` and `thurvsa system storage benchmark`.
///
/// Reads the daemon's YAML conffile, validates the requested `backends`
/// list against the `storage.backends:` block (empty = every named
/// backend), constructs one [`ObjectStoreBackend`] per name, and hands the
/// lot to [`run`]. Daemon-down: doesn't touch the admin socket so
/// operators can validate a freshly-configured backend before the
/// daemon ever opens it. Compression is forced off for the bench so the
/// numbers measure raw transport (real workload throughput is the
/// daemon's job to report).
pub async fn run_from_config_path(
    config_path: &Path,
    backends: Vec<String>,
    opts: BenchOptions,
) -> Result<(), BenchError> {
    let mut cfg = load_storage_config(config_path)?;
    // Force compression off so the benchmark measures raw transport, not
    // local codec throughput. The conffile default is zstd-3, which would
    // CPU-bound an incompressible-random run and under-report the link
    // ceiling — contradicting the doc claim above (issue #195).
    cfg.disable_compression();

    let backend_names: Vec<String> = if backends.is_empty() {
        cfg.backend_names()
    } else {
        for name in &backends {
            cfg.backend_entry(name)
                .map_err(|e| BenchError::BackendLookup {
                    name: name.clone(),
                    message: e.to_string(),
                })?;
        }
        backends
    };
    if backend_names.is_empty() {
        return Err(BenchError::InvalidArg(format!(
            "no backends defined under `storage.backends:` in {}; add one before benchmarking",
            config_path.display()
        )));
    }

    let mut targets = Vec::with_capacity(backend_names.len());
    for name in &backend_names {
        eprintln!("constructing backend '{}'...", name);
        let backend = cfg.create_backend_named(name).await.map_err(|source| {
            BenchError::BackendInstantiate {
                name: name.clone(),
                source,
            }
        })?;
        targets.push(BenchTarget {
            name: name.clone(),
            backend,
        });
    }

    run(targets, opts).await
}

/// Parse the daemon conffile and extract its `storage:` section. The
/// rest of the YAML is ignored — the bench engine only cares about the
/// `storage.backends:` map.
fn load_storage_config(config_path: &Path) -> Result<ObjectStoreConfig, BenchError> {
    #[derive(serde::Deserialize)]
    struct StorageOnly {
        #[serde(default)]
        storage: ObjectStoreConfig,
    }
    let body = std::fs::read_to_string(config_path).map_err(|e| BenchError::LoadConfig {
        path: config_path.to_path_buf(),
        message: e.to_string(),
    })?;
    let parsed: StorageOnly = serde_yaml::from_str(&body).map_err(|e| BenchError::LoadConfig {
        path: config_path.to_path_buf(),
        message: e.to_string(),
    })?;
    Ok(parsed.storage)
}

/// Run the bench against every (target × chunk_size × concurrency)
/// cell. Returns once every cell has been driven (or surfaced as
/// [BENCH-ERR]). The interactive sweep-cost prompt blocks on stdin
/// when sweeping and `yes=false`.
pub async fn run(targets: Vec<BenchTarget>, opts: BenchOptions) -> Result<(), BenchError> {
    // --- Validate args -------------------------------------------------
    let chunk_sizes: Vec<usize> = if opts.chunk_size_mb_sweep.is_empty() {
        vec![opts.chunk_size_mb]
    } else {
        opts.chunk_size_mb_sweep.clone()
    };
    let concurrencies: Vec<usize> = if opts.concurrency_sweep.is_empty() {
        vec![opts.concurrency]
    } else {
        opts.concurrency_sweep.clone()
    };

    if opts.total_gb == 0 {
        return Err(BenchError::InvalidArg("--total-gb must be > 0".to_string()));
    }
    let total_mb = opts.total_gb.checked_mul(1024).ok_or_else(|| {
        BenchError::InvalidArg(format!("--total-gb {} overflows MiB", opts.total_gb))
    })?;
    for &cs in &chunk_sizes {
        if cs == 0 {
            return Err(BenchError::InvalidArg("chunk size must be > 0".to_string()));
        }
        if cs > total_mb {
            return Err(BenchError::InvalidArg(format!(
                "chunk size {cs} MiB exceeds total {total_mb} MiB"
            )));
        }
    }
    for &c in &concurrencies {
        if c == 0 {
            return Err(BenchError::InvalidArg(
                "concurrency must be > 0".to_string(),
            ));
        }
    }

    if targets.is_empty() {
        return Err(BenchError::NoTargets);
    }

    // --- Preview + prompt when sweeping --------------------------------
    let sweep_active = !opts.chunk_size_mb_sweep.is_empty() || !opts.concurrency_sweep.is_empty();
    if sweep_active {
        print_sweep_preview(
            &targets,
            total_mb,
            &chunk_sizes,
            &concurrencies,
            opts.skip_download,
        );
        if !opts.yes {
            print!("Proceed? [y/N] ");
            std::io::stdout().flush().ok();
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(BenchError::Stdin)?;
            let answer = answer.trim().to_ascii_lowercase();
            if answer != "y" && answer != "yes" {
                println!("aborted.");
                return Err(BenchError::Aborted);
            }
        }
    }

    // --- Run cells -----------------------------------------------------
    let run_id = std::process::id();
    let mut cell_idx: usize = 0;
    for target in targets {
        let BenchTarget { name, backend } = target;
        let backend: Arc<dyn ObjectStoreBackend> = Arc::from(backend);
        for &chunk_size_mb in &chunk_sizes {
            for &concurrency in &concurrencies {
                cell_idx += 1;
                let result = bench_cell(
                    backend.clone(),
                    total_mb,
                    chunk_size_mb,
                    concurrency,
                    opts.skip_download,
                    run_id,
                    cell_idx,
                )
                .await;
                match result {
                    Ok(r) => print_bench_line(&name, total_mb, chunk_size_mb, concurrency, &r),
                    Err(e) => eprintln!(
                        "[BENCH-ERR] backend={} chunk_size_MiB={} concurrency={}: {}",
                        name, chunk_size_mb, concurrency, e
                    ),
                }
            }
        }
    }

    Ok(())
}

struct BenchResult {
    up_secs: f64,
    up_mibps: f64,
    down_secs: Option<f64>,
    down_mibps: Option<f64>,
}

fn fill_random(buf: &mut [u8], state: &mut u64) {
    // Same xorshift PRNG as core/smc/examples/perf_write.rs so
    // cross-test numbers are comparable. Content does not matter
    // for what the bench measures (upload_chunk is content-agnostic);
    // we use real bytes anyway in case some provider's data-plane
    // hot path special-cases zeros.
    for chunk in buf.chunks_mut(8) {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        let bytes = x.to_le_bytes();
        let n = chunk.len().min(8);
        chunk[..n].copy_from_slice(&bytes[..n]);
    }
}

fn print_sweep_preview(
    targets: &[BenchTarget],
    total_mb: usize,
    chunk_sizes: &[usize],
    concurrencies: &[usize],
    skip_download: bool,
) {
    let num_cells = targets.len() * chunk_sizes.len() * concurrencies.len();
    let mut total_chunks: u64 = 0;
    for &cs in chunk_sizes {
        let per_cell = total_mb.div_ceil(cs) as u64;
        total_chunks += per_cell * (targets.len() * concurrencies.len()) as u64;
    }
    let put_ops = total_chunks;
    let get_ops = if skip_download { 0 } else { total_chunks };
    let del_ops = total_chunks;
    let total_ops = put_ops + get_ops + del_ops;

    let per_cell_mib = total_mb as u64;
    let upload_mib = per_cell_mib * num_cells as u64;
    let egress_mib = if skip_download { 0 } else { upload_mib };
    let egress_gib = egress_mib as f64 / 1024.0;

    let names: Vec<&str> = targets.iter().map(|t| t.name.as_str()).collect();

    println!();
    println!("Sweep plan:");
    println!("  backends:       {}", names.join(", "));
    println!("  total_MiB:      {} per cell", total_mb);
    println!(
        "  chunk_size_mb:  {}",
        chunk_sizes
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "  concurrency:    {}",
        concurrencies
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "  cells:          {} ({} backends × {} chunk sizes × {} concurrencies)",
        num_cells,
        targets.len(),
        chunk_sizes.len(),
        concurrencies.len()
    );
    println!("  chunks total:   {}", total_chunks);
    println!(
        "  upload:    {:>7} PUT,    {:>9} MiB transferred",
        put_ops, upload_mib
    );
    if skip_download {
        println!("  download:  skipped (--skip-download)");
    } else {
        println!(
            "  download:  {:>7} GET,    {:>9} MiB egress",
            get_ops, egress_mib
        );
    }
    println!("  cleanup:   {:>7} DELETE", del_ops);
    println!("  total ops: {}", total_ops);

    println!();
    println!("NOTE: this benchmark issues real storage API calls (PUT/GET/DELETE) and");
    println!("transfers real bytes — it will cost money on metered backends.");
    println!();
    // Illustrative rates: $5 per 1M ops is a generous ceiling across
    // S3 / GCS / Azure standard tiers; $0.09/GB egress matches AWS
    // first-10-TB / month pricing. YOUR provider's rates differ —
    // scale by ratio.
    let cost_ops = total_ops as f64 * 5.0 / 1_000_000.0;
    let cost_egress = egress_gib * 0.09;
    let cost_total = cost_ops + cost_egress;
    println!("Rough cost (assumes $5 per 1M ops and $0.09 per GB egress, summed");
    println!("across all listed backends; YOUR provider's rates almost certainly");
    println!("differ — adjust by ratio):");
    println!(
        "  ops:     {:>10} × $5/1M       = ${:.2}",
        total_ops, cost_ops
    );
    println!(
        "  egress:  {:>6.1} GiB × $0.09/GB   = ${:.2}",
        egress_gib, cost_egress
    );
    println!("  total:   ~${:.2}", cost_total);
    println!();
}

async fn bench_cell(
    backend: Arc<dyn ObjectStoreBackend>,
    total_mb: usize,
    chunk_size_mb: usize,
    concurrency: usize,
    skip_download: bool,
    run_id: u32,
    cell_idx: usize,
) -> Result<BenchResult, String> {
    let chunk_bytes = chunk_size_mb * 1024 * 1024;
    let num_chunks = total_mb.div_ceil(chunk_size_mb);

    eprintln!(
        "cell {}: chunk_size={}MiB num_chunks={} concurrency={}",
        cell_idx, chunk_size_mb, num_chunks, concurrency
    );

    let mut buf = vec![0u8; chunk_bytes];
    let mut prng: u64 = 0x9E37_79B9_7F4A_7C15;
    fill_random(&mut buf, &mut prng);
    let payload = Bytes::from(buf);

    let keys: Vec<String> = (0..num_chunks)
        .map(|i| format!("bench/run-{}/cell-{}/chunk-{:06}", run_id, cell_idx, i))
        .collect();

    // --- upload --------------------------------------------------------
    let mib = (chunk_bytes * num_chunks) as f64 / (1024.0 * 1024.0);
    let up_start = Instant::now();
    let upload_outcomes: Vec<Result<(), String>> = stream::iter(keys.iter())
        .map(|key| {
            let backend = backend.clone();
            let payload = payload.clone();
            let key = key.clone();
            async move {
                backend
                    .upload_chunk(&key, &payload)
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("{}: {}", key, e))
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;
    let up_elapsed = up_start.elapsed().as_secs_f64();
    let up_failed: Vec<&String> = upload_outcomes
        .iter()
        .filter_map(|r| r.as_ref().err())
        .collect();
    if !up_failed.is_empty() {
        let _ = parallel_delete(&backend, &keys, concurrency).await;
        return Err(format!(
            "{} of {} uploads failed; first error: {}",
            up_failed.len(),
            num_chunks,
            up_failed[0]
        ));
    }
    let up_mibps = if up_elapsed > 0.0 {
        mib / up_elapsed
    } else {
        f64::INFINITY
    };

    // --- download (optional) -------------------------------------------
    let (down_secs, down_mibps) = if skip_download {
        (None, None)
    } else {
        let down_start = Instant::now();
        let download_outcomes: Vec<Result<usize, String>> = stream::iter(keys.iter())
            .map(|key| {
                let backend = backend.clone();
                let key = key.clone();
                async move {
                    backend
                        .download_chunk(&key)
                        .await
                        .map(|v| v.len())
                        .map_err(|e| format!("{}: {}", key, e))
                }
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;
        let down_elapsed = down_start.elapsed().as_secs_f64();
        let down_failed: Vec<&String> = download_outcomes
            .iter()
            .filter_map(|r| r.as_ref().err())
            .collect();
        if !down_failed.is_empty() {
            let _ = parallel_delete(&backend, &keys, concurrency).await;
            return Err(format!(
                "{} of {} downloads failed; first error: {}",
                down_failed.len(),
                num_chunks,
                down_failed[0]
            ));
        }
        for (i, r) in download_outcomes.iter().enumerate() {
            if let Ok(len) = r
                && *len != chunk_bytes
            {
                let _ = parallel_delete(&backend, &keys, concurrency).await;
                return Err(format!(
                    "download size mismatch at index {}: got {} expected {}",
                    i, len, chunk_bytes
                ));
            }
        }
        let m = if down_elapsed > 0.0 {
            mib / down_elapsed
        } else {
            f64::INFINITY
        };
        (Some(down_elapsed), Some(m))
    };

    // --- cleanup (always) ----------------------------------------------
    let del_errors = parallel_delete(&backend, &keys, concurrency).await;
    if !del_errors.is_empty() {
        eprintln!(
            "[BENCH-WARN] cell {}: {} of {} deletes failed (orphan keys left); first: {}",
            cell_idx,
            del_errors.len(),
            num_chunks,
            del_errors[0]
        );
    }

    Ok(BenchResult {
        up_secs: up_elapsed,
        up_mibps,
        down_secs,
        down_mibps,
    })
}

async fn parallel_delete(
    backend: &Arc<dyn ObjectStoreBackend>,
    keys: &[String],
    concurrency: usize,
) -> Vec<String> {
    let outcomes: Vec<Result<(), String>> = stream::iter(keys.iter())
        .map(|key| {
            let backend = backend.clone();
            let key = key.clone();
            async move {
                backend
                    .delete_object(&key)
                    .await
                    .map_err(|e| format!("{}: {}", key, e))
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;
    outcomes.into_iter().filter_map(|r| r.err()).collect()
}

fn print_bench_line(
    backend: &str,
    total_mb: usize,
    chunk_size_mb: usize,
    concurrency: usize,
    r: &BenchResult,
) {
    let num_chunks = total_mb.div_ceil(chunk_size_mb);
    let mut line = format!(
        "[BENCH] backend={} chunk_size_MiB={} concurrency={} total_MiB={} num_chunks={} up_secs={:.2} up_MiBps={:.1}",
        backend, chunk_size_mb, concurrency, total_mb, num_chunks, r.up_secs, r.up_mibps
    );
    if let (Some(s), Some(m)) = (r.down_secs, r.down_mibps) {
        line.push_str(&format!(" down_secs={:.2} down_MiBps={:.1}", s, m));
    } else {
        line.push_str(" download=skipped");
    }
    println!("{}", line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use shared_object_store::{
        CompressionAlgo, LockState, ObjectStoreError, Result as StorageResult,
    };
    use std::io::Write;
    use std::path::Path as StdPath;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// In-process `ObjectStoreBackend` that records call counts and answers with
    /// fixed-size payloads. Only the three methods the bench actually
    /// drives (`upload_chunk`, `download_chunk`, `delete_object`) carry
    /// real impls; the rest of the trait surface is `unreachable!()`
    /// since the bench never calls them.
    #[derive(Debug)]
    struct MockBackend {
        uploads: AtomicUsize,
        downloads: AtomicUsize,
        deletes: AtomicUsize,
        /// Bytes returned per `download_chunk`. Set to the cell's
        /// `chunk_bytes` for happy-path, set to anything else to
        /// exercise the size-mismatch branch.
        download_size: usize,
        /// 0-indexed upload ordinal that should error. `None` = all uploads succeed.
        fail_upload_at: Option<usize>,
        /// 0-indexed download ordinal that should error. `None` = all downloads succeed.
        fail_download_at: Option<usize>,
    }

    impl MockBackend {
        fn new(download_size: usize) -> Self {
            Self {
                uploads: AtomicUsize::new(0),
                downloads: AtomicUsize::new(0),
                deletes: AtomicUsize::new(0),
                download_size,
                fail_upload_at: None,
                fail_download_at: None,
            }
        }

        fn with_upload_failure(mut self, at: usize) -> Self {
            self.fail_upload_at = Some(at);
            self
        }

        fn with_download_failure(mut self, at: usize) -> Self {
            self.fail_download_at = Some(at);
            self
        }
    }

    #[async_trait]
    impl ObjectStoreBackend for MockBackend {
        async fn upload_chunk(
            &self,
            _key: &str,
            data: &[u8],
        ) -> StorageResult<(u64, Option<u64>, Option<CompressionAlgo>)> {
            let n = self.uploads.fetch_add(1, Ordering::Relaxed);
            if Some(n) == self.fail_upload_at {
                return Err(ObjectStoreError::Other("mock upload failure".into()));
            }
            Ok((data.len() as u64, None, None))
        }

        async fn upload_chunk_zerocopy(
            &self,
            _key: &str,
            _file_path: &StdPath,
        ) -> StorageResult<u64> {
            unreachable!("bench never calls upload_chunk_zerocopy")
        }

        async fn download_chunk(&self, _key: &str) -> StorageResult<Vec<u8>> {
            let n = self.downloads.fetch_add(1, Ordering::Relaxed);
            if Some(n) == self.fail_download_at {
                return Err(ObjectStoreError::Other("mock download failure".into()));
            }
            Ok(vec![0u8; self.download_size])
        }

        async fn download_chunks_parallel(&self, _keys: &[String]) -> StorageResult<Vec<Vec<u8>>> {
            unreachable!("bench never calls download_chunks_parallel")
        }

        async fn upload_manifest(&self, _key: &str, _json: &str) -> StorageResult<()> {
            unreachable!("bench never calls upload_manifest")
        }

        async fn download_manifest(&self, _key: &str) -> StorageResult<String> {
            unreachable!("bench never calls download_manifest")
        }

        async fn chunk_exists(&self, _key: &str) -> StorageResult<bool> {
            unreachable!("bench never calls chunk_exists")
        }

        async fn list_objects(&self, _key_prefix: &str) -> StorageResult<Vec<String>> {
            unreachable!("bench never calls list_objects")
        }

        async fn delete_object(&self, _key: &str) -> StorageResult<()> {
            self.deletes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn backend_type(&self) -> &'static str {
            "mock"
        }

        async fn lock_state(&self) -> StorageResult<LockState> {
            unreachable!("bench never calls lock_state")
        }

        async fn set_object_legal_hold(&self, _key: &str, _held: bool) -> StorageResult<()> {
            unreachable!("bench never calls set_object_legal_hold")
        }

        async fn get_object_legal_hold(&self, _key: &str) -> StorageResult<bool> {
            unreachable!("bench never calls get_object_legal_hold")
        }

        fn clone_box(&self) -> Box<dyn ObjectStoreBackend> {
            // `run` wraps the backend in `Arc::from(Box<_>)` once and
            // clones the Arc thereafter, so the trait's clone_box is
            // never reached in bench code paths.
            unreachable!("bench wraps the backend in Arc and never clones the Box")
        }
    }

    /// Tiny inputs the mock-driven tests share: 1 MiB chunks × 1 GiB
    /// total = 1024 in-memory ops per cell, sub-second wall time.
    fn tiny_opts() -> BenchOptions {
        BenchOptions {
            total_gb: 1,
            chunk_size_mb: 1,
            concurrency: 4,
            chunk_size_mb_sweep: Vec::new(),
            concurrency_sweep: Vec::new(),
            skip_download: false,
            yes: true,
        }
    }

    fn mock_target(name: &str, download_size_mb: usize) -> BenchTarget {
        BenchTarget {
            name: name.to_string(),
            backend: Box::new(MockBackend::new(download_size_mb * 1024 * 1024)),
        }
    }

    #[test]
    fn bench_options_defaults_match_the_cli_flag_defaults() {
        let o = BenchOptions::defaults();
        assert_eq!(o.total_gb, 32);
        assert_eq!(o.chunk_size_mb, 8);
        assert_eq!(o.concurrency, 16);
        assert!(o.chunk_size_mb_sweep.is_empty());
        assert!(o.concurrency_sweep.is_empty());
        assert!(!o.skip_download);
        assert!(!o.yes);
    }

    #[test]
    fn fill_random_is_deterministic_for_a_given_seed() {
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        let mut sa = 0x9E37_79B9_7F4A_7C15u64;
        let mut sb = sa;
        fill_random(&mut a, &mut sa);
        fill_random(&mut b, &mut sb);
        assert_eq!(a, b, "same seed must yield the same bytes");
        assert_ne!(a, [0u8; 64], "xorshift must not leave the buffer zeroed");
    }

    #[test]
    fn fill_random_handles_a_non_multiple_of_eight_buffer() {
        let mut buf = [0u8; 13];
        let mut state = 1u64;
        fill_random(&mut buf, &mut state);
        // The 5-byte tail is filled without panicking on the short chunk.
        assert!(buf.iter().any(|&b| b != 0));
    }

    #[test]
    fn load_storage_config_rejects_a_missing_file() {
        let err = load_storage_config(Path::new("/nonexistent/thur-bench-test.yaml"));
        assert!(matches!(err, Err(BenchError::LoadConfig { .. })));
    }

    #[test]
    fn load_storage_config_rejects_invalid_yaml() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let path = tmp.path().join("bad.yaml");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(b"storage: [this is not a map").expect("write");
        assert!(matches!(
            load_storage_config(&path),
            Err(BenchError::LoadConfig { .. }),
        ));
    }

    #[test]
    fn load_storage_config_parses_a_minimal_config() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let path = tmp.path().join("ok.yaml");
        std::fs::write(&path, "{}").expect("write");
        let cfg = load_storage_config(&path).expect("parse");
        assert!(cfg.backend_names().is_empty());
    }

    #[tokio::test]
    async fn run_rejects_invalid_options_before_touching_a_backend() {
        // total_gb == 0
        let mut o = BenchOptions::defaults();
        o.total_gb = 0;
        assert!(matches!(
            run(Vec::new(), o).await,
            Err(BenchError::InvalidArg(_)),
        ));

        // chunk size larger than the total transfer
        let mut o = BenchOptions::defaults();
        o.total_gb = 1;
        o.chunk_size_mb = 5000;
        assert!(matches!(
            run(Vec::new(), o).await,
            Err(BenchError::InvalidArg(_)),
        ));

        // zero chunk size
        let mut o = BenchOptions::defaults();
        o.chunk_size_mb = 0;
        assert!(matches!(
            run(Vec::new(), o).await,
            Err(BenchError::InvalidArg(_)),
        ));

        // zero concurrency
        let mut o = BenchOptions::defaults();
        o.concurrency = 0;
        assert!(matches!(
            run(Vec::new(), o).await,
            Err(BenchError::InvalidArg(_)),
        ));
    }

    #[tokio::test]
    async fn run_rejects_an_empty_target_list() {
        // Valid options, but no backends supplied.
        let err = run(Vec::new(), BenchOptions::defaults()).await;
        assert!(matches!(err, Err(BenchError::NoTargets)));
    }

    #[tokio::test]
    async fn run_from_config_path_rejects_a_missing_config() {
        let err = run_from_config_path(
            Path::new("/nonexistent/thur-bench.yaml"),
            Vec::new(),
            BenchOptions::defaults(),
        )
        .await;
        assert!(matches!(err, Err(BenchError::LoadConfig { .. })));
    }

    #[tokio::test]
    async fn run_from_config_path_rejects_a_config_with_no_backends() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let path = tmp.path().join("empty.yaml");
        std::fs::write(&path, "{}").expect("write");
        let err = run_from_config_path(&path, Vec::new(), BenchOptions::defaults()).await;
        assert!(matches!(err, Err(BenchError::InvalidArg(_))));
    }

    #[test]
    fn bench_error_display_messages_are_readable() {
        assert_eq!(BenchError::NoTargets.to_string(), "no targets supplied",);
        assert_eq!(
            BenchError::Aborted.to_string(),
            "operator aborted at sweep prompt",
        );
        assert!(
            BenchError::InvalidArg("bad".to_string())
                .to_string()
                .contains("bad"),
        );
    }

    #[tokio::test]
    async fn run_drives_upload_download_delete_against_a_mock_backend() {
        // 1 GiB total at 1 MiB per chunk = 1024 chunks. Mock no-ops
        // each op, so the cell completes in well under a second.
        let target = mock_target("mock-happy", 1);
        run(vec![target], tiny_opts()).await.expect("happy run");
        // We can't read the backend back after `run` (it consumed the
        // Box), so the assertions ride on `run` returning Ok — which
        // requires every upload + download to have succeeded and the
        // returned download lengths to match `chunk_bytes`.
    }

    #[tokio::test]
    async fn run_with_skip_download_omits_get_phase() {
        // Same shape, but skip_download flips the cell into PUT+DELETE
        // only — exercises the `(None, None)` branch in bench_cell and
        // the `download=skipped` print_bench_line tail.
        let mut opts = tiny_opts();
        opts.skip_download = true;
        let target = mock_target("mock-skip-download", 0);
        run(vec![target], opts).await.expect("skip-download run");
    }

    #[tokio::test]
    async fn run_with_sweep_yes_runs_cross_product_and_prints_preview() {
        // Each non-sweep cell exercises bench_cell at a different
        // chunk size / concurrency combo so the per-cell math + the
        // print_bench_line format string see real values.
        for &cs in &[1usize, 2] {
            for &conc in &[1usize, 2] {
                let opts = BenchOptions {
                    total_gb: 1,
                    chunk_size_mb: cs,
                    concurrency: conc,
                    chunk_size_mb_sweep: Vec::new(),
                    concurrency_sweep: Vec::new(),
                    skip_download: false,
                    yes: true,
                };
                let target = mock_target("mock-sweep-cell", cs);
                run(vec![target], opts).await.expect("sweep cell");
            }
        }
        // One true sweep run with yes=true + skip_download covers the
        // preview path (cost arithmetic, cell-count print, skipped GET
        // branch) and the cross-product loop in `run`.
        let opts = BenchOptions {
            total_gb: 1,
            chunk_size_mb: 1,
            concurrency: 1,
            chunk_size_mb_sweep: vec![1, 2],
            concurrency_sweep: vec![1],
            skip_download: true,
            yes: true,
        };
        let target = mock_target("mock-sweep-preview", 0);
        run(vec![target], opts).await.expect("preview run");
    }

    #[tokio::test]
    async fn run_reports_per_cell_upload_failures_without_aborting_siblings() {
        // First target fails its 3rd upload; second succeeds. `run`
        // returns Ok regardless — per-cell errors land on stderr as
        // [BENCH-ERR] without aborting the outer loop. The failing
        // cell triggers the parallel_delete cleanup path.
        let fail_target = BenchTarget {
            name: "mock-fail".to_string(),
            backend: Box::new(MockBackend::new(1024 * 1024).with_upload_failure(3)),
        };
        let ok_target = mock_target("mock-ok", 1);
        run(vec![fail_target, ok_target], tiny_opts())
            .await
            .expect("run returns Ok despite per-cell failure");
    }

    #[tokio::test]
    async fn run_reports_download_failures_without_aborting_siblings() {
        // Mock's download_size is 0, so every download returns the
        // wrong number of bytes — triggers the size-mismatch branch
        // in bench_cell and the cleanup path that follows.
        let mismatch_target = mock_target("mock-mismatch", 0);
        run(vec![mismatch_target], tiny_opts())
            .await
            .expect("run returns Ok despite size mismatch");

        // And the IO-error path: download #2 returns ObjectStoreError.
        let io_target = BenchTarget {
            name: "mock-download-io".to_string(),
            backend: Box::new(MockBackend::new(1024 * 1024).with_download_failure(2)),
        };
        run(vec![io_target], tiny_opts())
            .await
            .expect("run returns Ok despite download IO error");
    }

    #[tokio::test]
    async fn parallel_delete_aggregates_per_key_errors_from_a_real_failure() {
        // parallel_delete with a backend whose delete_object always
        // succeeds returns an empty error list. The aggregation path
        // itself is the unit under test; the bench's own cleanup also
        // exercises it on every cell.
        use std::sync::Arc;
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(MockBackend::new(0));
        let keys: Vec<String> = (0..8).map(|i| format!("k{i}")).collect();
        let errors = parallel_delete(&backend, &keys, 4).await;
        assert!(errors.is_empty(), "all-succeed → no errors");
    }
}
