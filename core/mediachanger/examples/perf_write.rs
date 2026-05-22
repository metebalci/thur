// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Streaming-write throughput benchmark — configurable fixture + chunking.
//
// Usage:
//   cargo run --release -p core-mediachanger --example perf_write -- \
//     <mode> <total_mb> <block_kib> [fixture]
// where:
//   mode      = fixed-128 | fastcdc | fastcdc-128
//   total_mb  = total bytes to write (MiB)
//   block_kib = iSCSI-style block size handed to write_data (KiB), e.g. 256
//   fixture   = random | compressible (default random)
//
// Reports end-to-end throughput including chunk seal & fsync. The
// random fixture uses a deterministic xorshift PRNG so cross-runs are
// comparable but the bytes are not artificially compressible/dedupable.
// The compressible fixture is 50% zeros + 50% repeating
// `ABCDEFGHIJKLMNOP` and matches the perf-layers harness's shell-side
// fixture so VTL L1 vs L3 numbers compare on the same workload.

use bytes::Bytes;
use core_mediachanger::{Cartridge, ChunkingMode, DedupScope};
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("fastcdc");
    let total_mb: usize = args
        .get(2)
        .map(|s| s.parse().unwrap_or(1024))
        .unwrap_or(1024);
    let block_kib: usize = args.get(3).map(|s| s.parse().unwrap_or(256)).unwrap_or(256);
    let fixture = args.get(4).map(String::as_str).unwrap_or("random");
    if fixture != "random" && fixture != "compressible" {
        return Err(format!("unknown fixture: {} (want random|compressible)", fixture).into());
    }

    let chunking = match mode {
        "fixed-128" => ChunkingMode::Fixed {
            size_bytes: 128 * 1024 * 1024,
        },
        "fastcdc" => ChunkingMode::FastCdc {
            min: 1024 * 1024,
            avg: 8 * 1024 * 1024,
            max: 32 * 1024 * 1024,
        },
        "fastcdc-128" => ChunkingMode::FastCdc {
            min: 16 * 1024 * 1024,
            avg: 128 * 1024 * 1024,
            max: 512 * 1024 * 1024,
        },
        other => return Err(format!("unknown mode: {}", other).into()),
    };

    let tmp = tempfile::tempdir()?;
    let tapes_dir = tmp.path().join("tapes");
    std::fs::create_dir_all(&tapes_dir)?;

    println!(
        "perf_write: mode={mode} total={total_mb}MiB block={block_kib}KiB \
         fixture={fixture} chunking={:?}",
        chunking
    );

    let mut cart = Cartridge::create_with_chunking(
        &tapes_dir,
        "PERF001",
        chunking,
        9,
        "primary",
        false,
        DedupScope::Local,
    )?;

    let block_bytes = block_kib * 1024;
    let total_bytes: usize = total_mb * 1024 * 1024;
    let num_blocks = total_bytes / block_bytes;
    let mut buf = vec![0u8; block_bytes];
    let mut prng_state: u64 = 0x9E3779B97F4A7C15;
    let half_bytes = (total_bytes as u64) / 2;
    let mut byte_offset: u64 = 0;

    // Print per-segment instantaneous throughput so we can see whether
    // per-write cost grows with cartridge fill (O(N) leak) or stays flat
    // (constant per-write, just slow).
    let segment_mb: usize = 128;
    let blocks_per_segment = (segment_mb * 1024 * 1024) / block_bytes;

    let start = Instant::now();
    let mut seg_start = Instant::now();
    let mut blocks_in_seg = 0usize;
    let mut seg_idx = 0usize;
    for _ in 0..num_blocks {
        if fixture == "compressible" {
            fill_compressible(&mut buf, byte_offset, half_bytes);
        } else {
            fill_random(&mut buf, &mut prng_state);
        }
        cart.write_data(Bytes::copy_from_slice(&buf))?;
        byte_offset += block_bytes as u64;
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

    // Force trailing seal + fsync so we capture the full cost.
    let seal_start = Instant::now();
    cart.flush_and_seal()?;
    let seal_elapsed = seal_start.elapsed();

    let total_secs = write_elapsed.as_secs_f64() + seal_elapsed.as_secs_f64();
    let mb = total_bytes as f64 / (1024.0 * 1024.0);
    println!(
        "WRITE  : {mb:.0} MiB in {total_secs:.2}s = {:.1} MiB/s (write={:.2}s seal={:.2}s)",
        mb / total_secs,
        write_elapsed.as_secs_f64(),
        seal_elapsed.as_secs_f64()
    );

    let chunks = cart.get_pending_uploads().len();
    println!(
        "CHUNKS : {chunks} sealed (avg {:.2} MiB)",
        mb / chunks.max(1) as f64
    );

    // Post-dedup pool size: walk the chunk pool on disk and sum
    // unique chunk file sizes. Drop the cartridge first so any
    // unsealed trailing chunk has been flushed into the pool.
    drop(cart);
    let mut pool_bytes: u64 = 0;
    let mut pool_files: u64 = 0;
    let pool_root = tapes_dir
        .parent()
        .ok_or("tapes_dir has no parent")?
        .join("chunks");
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
        "DEDUP  : {pool_files} unique chunk files = {pool_mb:.1} MiB on disk \
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
