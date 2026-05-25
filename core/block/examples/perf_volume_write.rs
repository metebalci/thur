// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// VSA L1 streaming-write throughput benchmark — random data, in-process,
// LocalBackend only. Counterpart of core/smc/examples/perf_write.rs on the
// VTL side; the perf-layers harness compares this row against the higher
// layers (L2 = +cloud, L3 = iSCSI raw, L4 = iSCSI+ext4).
//
// Usage:
//   cargo run --release -p core-block --example perf_volume_write -- \
//     <total_mb> <block_kib> [fixture]
// where:
//   total_mb  = total bytes to write (MiB)
//   block_kib = host-write chunk size handed to write_bytes (KiB), e.g. 256
//   fixture   = random | compressible (default random)
//
// Reports end-to-end throughput including the trailing flush_all (which
// drains every dirty page through the chunk pool into LocalBackend). The
// random fixture uses the deterministic xorshift PRNG from perf_write.rs
// so cross-runs and cross-product (VSA L1 vs VTL L1) numbers are
// comparable but the bytes are not artificially compressible/dedupable;
// the compressible fixture is 50% zeros + 50% repeating
// `ABCDEFGHIJKLMNOP`, matching the perf-layers harness's shell-side
// fixture so L1 vs L3/L4 numbers compare on the same workload.

use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
use core_block::{DedupScope, PageCache, VolumeManifest, VolumeWriter};
use shared_object_store::{LocalBackend, ObjectStoreBackend};
use std::sync::Arc;
use std::time::Instant;

fn fill_random(buf: &mut [u8], state: &mut u64) {
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

// First half = all-zero bytes (max-compressible), second half = a
// 16-byte repeating ASCII pattern. Matches the shell-side compressible
// fixture written by perf-layers.sh for L3/L4 so the four-row table
// reports the same workload at every layer.
fn fill_compressible(buf: &mut [u8], offset: u64, half: u64) {
    const PAT: &[u8; 16] = b"ABCDEFGHIJKLMNOP";
    for (i, b) in buf.iter_mut().enumerate() {
        let abs = offset + i as u64;
        if abs < half {
            *b = 0;
        } else {
            *b = PAT[((abs - half) as usize) % PAT.len()];
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let total_mb: usize = args
        .get(1)
        .map(|s| s.parse().unwrap_or(1024))
        .unwrap_or(1024);
    let block_kib: usize = args.get(2).map(|s| s.parse().unwrap_or(256)).unwrap_or(256);
    let fixture = args.get(3).map(String::as_str).unwrap_or("random");
    if fixture != "random" && fixture != "compressible" {
        return Err(format!("unknown fixture: {} (want random|compressible)", fixture).into());
    }

    let tmp = tempfile::tempdir()?;
    let data_dir = tmp.path();

    println!(
        "perf_volume_write: total={total_mb}MiB block={block_kib}KiB fixture={fixture} \
         page={}KiB sector={}B",
        DEFAULT_PAGE_SIZE_BYTES / 1024,
        DEFAULT_SECTOR_BYTES
    );

    // LocalBackend lives under <data_dir>/local-cloud so the chunk pool
    // (which writes to <data_dir>/chunks/<backend>/...) is on the same
    // filesystem — matches the daemon's on-disk shape closely enough that
    // the L1 number is representative of a real LocalBackend deployment.
    let cloud_root = data_dir.join("local-cloud");
    std::fs::create_dir_all(&cloud_root)?;
    let backend = LocalBackend::new(&cloud_root).await?;
    let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);

    let total_bytes: usize = total_mb * 1024 * 1024;
    // Volume size = 2 * total_bytes gives headroom; sector-aligned by
    // construction since both are multiples of 1 MiB.
    let size_bytes = (total_bytes as u64) * 2;

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
    let cache = PageCache::new(writer.clone());

    let block_bytes = block_kib * 1024;
    let num_blocks = total_bytes / block_bytes;
    let mut buf = vec![0u8; block_bytes];
    let mut prng_state: u64 = 0x9E3779B97F4A7C15;
    let half_bytes = (total_bytes as u64) / 2;

    // Per-segment instantaneous throughput, same cadence as perf_write.rs
    // so the two examples can be eyeballed side-by-side.
    let segment_mb: usize = 128;
    let blocks_per_segment = (segment_mb * 1024 * 1024) / block_bytes;

    let start = Instant::now();
    let mut seg_start = Instant::now();
    let mut blocks_in_seg = 0usize;
    let mut seg_idx = 0usize;
    let mut offset: u64 = 0;
    for _ in 0..num_blocks {
        if fixture == "compressible" {
            fill_compressible(&mut buf, offset, half_bytes);
        } else {
            fill_random(&mut buf, &mut prng_state);
        }
        cache.write_bytes(offset, &buf).await?;
        offset += block_bytes as u64;
        blocks_in_seg += 1;
        if blocks_in_seg == blocks_per_segment {
            let secs = seg_start.elapsed().as_secs_f64();
            let mb = segment_mb as f64;
            seg_idx += 1;
            eprintln!(
                "  seg {seg_idx:2}: {mb:.0} MiB in {secs:.3}s = {:.1} MiB/s (cum {:.0} MiB)",
                mb / secs,
                (seg_idx * segment_mb) as f64
            );
            seg_start = Instant::now();
            blocks_in_seg = 0;
        }
    }
    let write_elapsed = start.elapsed();

    // flush_all drains every dirty page through the writer →
    // ChunkPool → LocalBackend. This captures the chunk-seal +
    // upload cost as a separate phase, mirroring perf_write's seal
    // line. It's the "in-process" half of the cloud upload — L2
    // replaces LocalBackend with a real backend to expose what the
    // network adds on top.
    let flush_start = Instant::now();
    cache.flush_all().await?;
    let flush_elapsed = flush_start.elapsed();

    let total_secs = write_elapsed.as_secs_f64() + flush_elapsed.as_secs_f64();
    let mb = total_bytes as f64 / (1024.0 * 1024.0);
    println!(
        "WRITE  : {mb:.0} MiB in {total_secs:.2}s = {:.1} MiB/s (write={:.2}s flush={:.2}s)",
        mb / total_secs,
        write_elapsed.as_secs_f64(),
        flush_elapsed.as_secs_f64()
    );

    // Drop the writer (and cache) so any trailing pool state is on
    // disk before we count chunk files.
    drop(cache);
    drop(writer);

    let mut pool_bytes: u64 = 0;
    let mut pool_files: u64 = 0;
    let pool_root = data_dir.join("chunks");
    for entry in walkdir(&pool_root) {
        if entry.is_file()
            && let Ok(meta) = std::fs::metadata(&entry)
        {
            pool_bytes += meta.len();
            pool_files += 1;
        }
    }
    let pool_mb = pool_bytes as f64 / (1024.0 * 1024.0);
    let dedup_ratio = if pool_bytes > 0 {
        total_bytes as f64 / pool_bytes as f64
    } else {
        0.0
    };
    println!(
        "CHUNKS : {pool_files} unique chunk files = {pool_mb:.1} MiB on disk \
         (logical {mb:.0} MiB, dedup ratio {dedup_ratio:.2}x)"
    );

    Ok(())
}

fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}
