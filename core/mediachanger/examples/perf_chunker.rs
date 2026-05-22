// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// FastCDC microbenchmark — measures pure chunker throughput on
// deterministic random input, isolating the rolling-hash inner loop
// from the rest of the cartridge write path.
//
// Usage:
//   cargo run --release -p core-mediachanger --example perf_chunker -- [total_mb] [block_kib]
//
// Reports:
//   - find_cut throughput (one-shot, large buffer)
//   - StreamingChunker::feed throughput at the configured iSCSI-block size
//   - cuts produced and mean chunk size
//
// The fixture is deterministic-random (xorshift) so re-runs are stable
// and dedup ratio is ~1.0 (we are measuring chunker work, not pool churn).

use core_mediachanger::fastcdc::{FastCdc, StreamingChunker};
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let total_mb: usize = args.get(1).map(|s| s.parse().unwrap_or(512)).unwrap_or(512);
    let block_kib: usize = args.get(2).map(|s| s.parse().unwrap_or(256)).unwrap_or(256);

    let cdc = FastCdc::default(); // 1/8/32 MiB
    println!(
        "perf_chunker: total={total_mb}MiB block={block_kib}KiB cdc=(min={} avg={} max={})",
        cdc.min, cdc.avg, cdc.max
    );

    let total_bytes = total_mb * 1024 * 1024;
    let mut data = vec![0u8; total_bytes];
    let mut prng = 0x9E3779B97F4A7C15u64;
    fill_random(&mut data, &mut prng);

    // 1) find_cut — chunk up the whole buffer using one-shot calls.
    let start = Instant::now();
    let mut offsets = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let cut = cdc.find_cut(&data[pos..]);
        if cut == 0 || cut == data.len() - pos {
            // No further cut found — trailing bytes go into the last chunk.
            break;
        }
        offsets.push(pos + cut);
        pos += cut;
    }
    let elapsed = start.elapsed();
    let mb = total_bytes as f64 / (1024.0 * 1024.0);
    println!(
        "find_cut       : {mb:.0} MiB in {:.3}s = {:.1} MiB/s ({} cuts, mean {:.2} MiB)",
        elapsed.as_secs_f64(),
        mb / elapsed.as_secs_f64(),
        offsets.len(),
        mb / offsets.len().max(1) as f64,
    );

    // 2) StreamingChunker::feed — iSCSI-style per-block calls.
    let block_bytes = block_kib * 1024;
    let mut sc = StreamingChunker::new(cdc);
    let start = Instant::now();
    let mut feed_cuts = 0usize;
    let mut consumed = 0usize;
    while consumed < data.len() {
        let end = (consumed + block_bytes).min(data.len());
        if sc.feed(&data[consumed..end]) {
            feed_cuts += 1;
            sc.reset();
        }
        consumed = end;
    }
    let elapsed = start.elapsed();
    println!(
        "feed (block={block_kib}K): {mb:.0} MiB in {:.3}s = {:.1} MiB/s ({} cuts, mean {:.2} MiB)",
        elapsed.as_secs_f64(),
        mb / elapsed.as_secs_f64(),
        feed_cuts,
        mb / feed_cuts.max(1) as f64,
    );

    // 3) StreamingChunker::feed — byte-by-byte (worst case for branch
    // overhead, useful upper bound on per-byte cost).
    let mut sc = StreamingChunker::new(cdc);
    let bytes_to_test = total_bytes.min(64 * 1024 * 1024); // cap: bbb is slow
    let start = Instant::now();
    let mut bbb_cuts = 0usize;
    for &b in &data[..bytes_to_test] {
        if sc.feed(std::slice::from_ref(&b)) {
            bbb_cuts += 1;
            sc.reset();
        }
    }
    let elapsed = start.elapsed();
    let bbb_mb = bytes_to_test as f64 / (1024.0 * 1024.0);
    println!(
        "feed (1B/call) : {bbb_mb:.0} MiB in {:.3}s = {:.1} MiB/s ({} cuts)",
        elapsed.as_secs_f64(),
        bbb_mb / elapsed.as_secs_f64(),
        bbb_cuts,
    );
}
