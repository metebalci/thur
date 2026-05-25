// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// VTL L2 streaming-write throughput benchmark — extends perf_write
// with an inline upload phase against a configured cloud backend.
// Plumbs Cartridge::write_data → flush_and_seal → <configured backend>;
// the harness-level "L2 − L1" delta exposes what the network upload
// costs on top of the in-process chunker.
//
// Usage:
//   cargo run --release -p core-mediachanger --example perf_cart_cloud -- \
//     <config.yaml> <backend_name> <mode> <total_mb> <block_kib> [fixture]
// where:
//   config.yaml   = path to a daemon YAML conffile with `backend_name`
//                   defined under `cloud.backends:`
//                   (local / S3 / GCS / Azure)
//   backend_name  = key under `cloud.backends:` to load
//   mode          = fixed-128 | fastcdc | fastcdc-128 (see perf_write.rs)
//   total_mb      = total bytes to write (MiB)
//   block_kib     = host-write block size handed to write_data (KiB)
//   fixture       = random | compressible (default random)
//
// Compression is forced off so the reported number isolates network
// upload from zstd compute — matches the `-none` backend naming
// convention used by the pipeline-layer matrix. Compressible-fixture
// shape (50% zeros + 50% repeating ABCDEFGHIJKLMNOP) matches the
// shell-side fixture for L3 so both rows compare at the same workload.

use bytes::Bytes;
use core_mediachanger::{Cartridge, ChunkingMode, DedupScope, upload_chunk_inert};
use shared_object_store::ObjectStoreConfig;
use std::path::PathBuf;
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let config_path: PathBuf = args
        .get(1)
        .ok_or("missing arg: <config.yaml>")?
        .clone()
        .into();
    let backend_name = args.get(2).ok_or("missing arg: <backend_name>")?.clone();
    let mode = args.get(3).map(String::as_str).unwrap_or("fastcdc");
    let total_mb: usize = args
        .get(4)
        .map(|s| s.parse().unwrap_or(1024))
        .unwrap_or(1024);
    let block_kib: usize = args.get(5).map(|s| s.parse().unwrap_or(256)).unwrap_or(256);
    let fixture = args.get(6).map(String::as_str).unwrap_or("random");
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
        "perf_cart_cloud: backend={backend_name} mode={mode} total={total_mb}MiB \
         block={block_kib}KiB fixture={fixture} chunking={:?}",
        chunking
    );

    let body = std::fs::read_to_string(&config_path)?;
    #[derive(serde::Deserialize)]
    struct StorageOnly {
        #[serde(default)]
        storage: ObjectStoreConfig,
    }
    let cfg: StorageOnly = serde_yaml::from_str(&body)?;
    let backend = cfg.storage.create_backend_named(&backend_name).await?;

    let mut cart = Cartridge::create_with_chunking(
        &tapes_dir,
        "PERF001",
        chunking,
        9,
        &backend_name,
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

    let seal_start = Instant::now();
    cart.flush_and_seal()?;
    let seal_elapsed = seal_start.elapsed();

    // Upload phase — walk every sealed chunk, push it through the
    // configured backend via upload_chunk_inert, apply the outcome
    // back into the cartridge's manifest. Mirrors the daemon's
    // upload-worker shape but sequential so the timing is a clean
    // single-stream upload measurement.
    let upload_start = Instant::now();
    let chunk_ids: Vec<u64> = cart
        .get_pending_uploads()
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    let pending_count = chunk_ids.len();
    for chunk_id in chunk_ids {
        let payload = cart
            .pending_upload_payload(chunk_id)
            .ok_or("pending_upload_payload returned None for sealed chunk")?;
        let outcome = upload_chunk_inert(&*backend, &payload).await?;
        cart.apply_chunk_upload_outcome(&outcome);
    }
    let upload_elapsed = upload_start.elapsed();

    let total_secs =
        write_elapsed.as_secs_f64() + seal_elapsed.as_secs_f64() + upload_elapsed.as_secs_f64();
    let mb = total_bytes as f64 / (1024.0 * 1024.0);
    println!(
        "WRITE  : {mb:.0} MiB in {total_secs:.2}s = {:.1} MiB/s \
         (write={:.2}s seal={:.2}s upload={:.2}s)",
        mb / total_secs,
        write_elapsed.as_secs_f64(),
        seal_elapsed.as_secs_f64(),
        upload_elapsed.as_secs_f64(),
    );
    println!(
        "CHUNKS : {pending_count} uploaded (avg {:.2} MiB each)",
        mb / pending_count.max(1) as f64
    );

    Ok(())
}
