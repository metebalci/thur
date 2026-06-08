// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// VSA page-cache READ-HIT microbenchmark — isolates the read path's
// per-hit cost: the page-body clone and the LRU "touch" bookkeeping.
// Counterpart of perf_volume_write.rs (which measures the write +
// flush path). Random data, in-process, LocalBackend only.
//
// Usage:
//   cargo run --release -p core-block --example perf_volume_read -- \
//     <working_set_mb> <num_reads> [read_kib]
// where:
//   working_set_mb = resident hot set (MiB). The cache budget is set
//                    to exactly this, so the whole set stays in RAM
//                    and every timed read is a cache HIT (no storage /
//                    pool round trip). This is also the LRU list
//                    length n — at the 64 KiB-page default,
//                    256 MiB = 4096 resident pages.
//   num_reads      = number of timed random-page reads.
//   read_kib       = bytes per read (KiB), default 4 (one sector).
//                    Sub-page reads make the page-body clone the
//                    dominant per-hit cost, so the Arc-vs-Vec change
//                    shows clearly; the request is clamped to the
//                    page size.
//
// What this benchmark is sensitive to (issue #51):
//   - per-hit page-body clone: a cache hit used to clone the whole
//     (64 KiB) page under the lock; with Arc<Vec<u8>> page bodies it
//     clones a refcount, independent of read_kib.
//   - LRU touch: each hit moves the page to the MRU end. The old
//     VecDeque did an O(n) scan+shift; the intrusive list is O(1).
//     A large working_set_mb (big n) is where the two diverge.
//
// Reads are pseudo-random over the resident set using the same
// deterministic xorshift PRNG as perf_volume_write.rs, so runs are
// stable and comparable.

use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
use core_block::{DedupScope, PageCache, VolumeManifest, VolumeWriter};
use shared_object_store::{LocalBackend, ObjectStoreBackend};
use std::sync::Arc;
use std::time::Instant;

fn fill_random(buf: &mut [u8], state: &mut u64) {
    for chunk in buf.chunks_mut(8) {
        let x = next_rand(state);
        let bytes = x.to_le_bytes();
        let n = chunk.len().min(8);
        chunk[..n].copy_from_slice(&bytes[..n]);
    }
}

fn next_rand(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let working_set_mb: usize = args.get(1).map(|s| s.parse().unwrap_or(256)).unwrap_or(256);
    let num_reads: usize = args
        .get(2)
        .map(|s| s.parse().unwrap_or(200_000))
        .unwrap_or(200_000);
    let read_kib: usize = args.get(3).map(|s| s.parse().unwrap_or(4)).unwrap_or(4);

    let page_size = DEFAULT_PAGE_SIZE_BYTES as usize;
    let read_len = (read_kib * 1024).min(page_size).max(1);

    let working_set_bytes = working_set_mb * 1024 * 1024;
    let working_set_pages = (working_set_bytes / page_size).max(1);

    let tmp = tempfile::tempdir()?;
    let data_dir = tmp.path();
    let storage_root = data_dir.join("local-storage");
    std::fs::create_dir_all(&storage_root)?;
    let backend = LocalBackend::new(&storage_root).await?;
    let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);

    // Volume exactly the size of the resident set; cache budget set so
    // every page stays resident (no eviction during the read phase).
    let size_bytes = working_set_bytes as u64;
    VolumeManifest::new(
        "perf001".into(),
        size_bytes,
        DEFAULT_SECTOR_BYTES,
        DEFAULT_PAGE_SIZE_BYTES,
        "primary".into(),
        DedupScope::Local,
        false,
        0,
    )?
    .create(data_dir)?;

    let writer = Arc::new(VolumeWriter::open(data_dir, "perf001", backend)?);
    let cache = PageCache::with_budget(writer.clone(), working_set_bytes as u64);

    println!(
        "perf_volume_read: working_set={working_set_mb}MiB ({working_set_pages} pages, \
         budget {} pages) reads={num_reads} read={}KiB page={}KiB",
        cache.budget_pages(),
        read_len / 1024,
        page_size / 1024,
    );

    // Warm the cache: write every page once so the whole working set
    // is resident (and dirty — irrelevant to the read path, which
    // serves both clean and dirty pages from the same in-memory body).
    let mut buf = vec![0u8; page_size];
    let mut prng_state: u64 = 0x9E3779B97F4A7C15;
    for p in 0..working_set_pages {
        fill_random(&mut buf, &mut prng_state);
        cache.write_bytes((p * page_size) as u64, &buf).await?;
    }

    // Timed read phase: num_reads random-page hits.
    let mut sink: u64 = 0;
    let start = Instant::now();
    for _ in 0..num_reads {
        let page = (next_rand(&mut prng_state) as usize) % working_set_pages;
        let off = (page * page_size) as u64;
        let bytes = cache.read_bytes(off, read_len).await?;
        // Touch the result so the read can't be optimized away.
        sink = sink.wrapping_add(u64::from(bytes.first().copied().unwrap_or(0)));
    }
    let elapsed = start.elapsed();

    let secs = elapsed.as_secs_f64();
    let reads_per_sec = num_reads as f64 / secs;
    let mib = (num_reads * read_len) as f64 / (1024.0 * 1024.0);
    println!(
        "READ-HIT: {num_reads} reads in {secs:.3}s = {reads_per_sec:.0} reads/s, \
         {:.1} MiB/s ({:.0} ns/read) [sink={sink}]",
        mib / secs,
        secs * 1e9 / num_reads as f64,
    );

    Ok(())
}
