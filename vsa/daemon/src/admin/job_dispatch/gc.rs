// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `system.gc` job — orphan-chunk garbage collection for the
//! content-addressed chunk pool.
//!
//! Block-side parallel of `vtl/daemon/src/admin/job_dispatch/gc.rs`.
//! Two differences from the tape side:
//!
//! - The live set comes from each volume's `pages.idx` (the
//!   `page_id -> chunk_hash` map), not a tape's chunk index.
//! - There is no cloud index-page sweep. VSA uploads only chunk
//!   objects (`chunks/[<ns>/]<aa>/<bb>/<hash>.dat`); it persists no
//!   `manifests/<...>/page-NNN` objects, so VTL's
//!   `run_cloud_index_pages_gc` has no analogue here.
//!
//! The namespace for a `Local`-scope volume is its UUID hex
//! ([`VolumeManifest::pool_namespace`]); `Global`-scope volumes share
//! the per-backend pool (namespace `None`).
//!
//! Body params: `{ "dry_run": bool, "cloud": bool }`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use core_block::{ChunkPool, PageIndex, VolumeManifest};
use serde::Deserialize;
use shared_admin_server::{JobEmitter, JobEvent};
use shared_audit::{AuditActor, AuditResult};
use shared_object_store::ObjectStoreConfig;
use tracing::warn;

use crate::admin::handlers::AdminState;

#[derive(Debug, Default, Deserialize)]
pub struct GcParams {
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub storage: bool,
}

/// Per-(backend, namespace) live-hash bucket. `namespace` is `None`
/// for `Global`-scope volumes (the shared pool), `Some(uuid-hex)` for
/// `Local`-scope volumes.
type LiveSet = HashMap<(String, Option<String>), HashSet<String>>;

pub async fn run(emitter: JobEmitter, body: serde_json::Value, state: AdminState) {
    let params: GcParams = match serde_json::from_value(body) {
        Ok(p) => p,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(2, format!("bad params: {}", e)))
                .await;
            return;
        }
    };

    let data_dir = state.data_dir.clone();

    // Phase 1 — collect the live set. Page-index iteration is sync
    // pread; run it on the blocking pool.
    let dd_for_collect = data_dir.clone();
    let live: Result<LiveSet, anyhow::Error> =
        tokio::task::spawn_blocking(move || collect_live_hashes(&dd_for_collect))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("collect panicked: {}", e)));
    let live = match live {
        Ok(l) => l,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(2, format!("collect: {}", e)))
                .await;
            return;
        }
    };

    let total_live: usize = live.values().map(|s| s.len()).sum();
    let backend_count: HashSet<&String> = live.keys().map(|(b, _)| b).collect();
    emitter
        .info(format!(
            "Live hashes referenced by volumes: {} across {} backend(s) ({} namespace(s))",
            total_live,
            backend_count.len(),
            live.len(),
        ))
        .await;
    emitter.info("").await;

    // Phase 2 — per-backend sweeps.
    let mut total_freed: u64 = 0;
    let mut local_summary: Vec<serde_json::Value> = Vec::new();
    let backend_names = state.storage.backend_names();
    for backend_name in &backend_names {
        emitter
            .info(format!("=== Backend: {} ===", backend_name))
            .await;

        // Local pool sweep on the blocking pool — chunk removals hit
        // fs::remove_file in a loop.
        let bn = backend_name.clone();
        let live_clone = live.clone();
        let dd = data_dir.clone();
        let dry = params.dry_run;
        let lines_with_freed =
            tokio::task::spawn_blocking(move || run_local_gc(&dd, &bn, &live_clone, dry))
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("local gc panicked: {}", e)));

        match lines_with_freed {
            Ok((lines, freed)) => {
                for line in lines {
                    emitter.info(line).await;
                }
                total_freed = total_freed.saturating_add(freed);
                local_summary.push(serde_json::json!({
                    "backend": backend_name,
                    "bytes_freed_local": freed,
                }));
            }
            Err(e) => {
                emitter
                    .error(format!("local gc on backend {}: {}", backend_name, e))
                    .await;
            }
        }

        if params.storage
            && let Err(e) = run_storage_gc(
                &emitter,
                &state.storage,
                backend_name,
                &live,
                params.dry_run,
            )
            .await
        {
            emitter
                .error(format!("cloud gc on backend {}: {}", backend_name, e))
                .await;
        }
        emitter.info("").await;
    }
    if !params.storage {
        emitter
            .info("(Skipping cloud GC — re-run with cloud:true to clean buckets too.)")
            .await;
        emitter.info("").await;
    }

    if params.dry_run {
        emitter.info("Dry-run only — no files were deleted.").await;
    } else {
        emitter
            .info(format!(
                "Local pool reclaimed: {} bytes ({:.2} MiB)",
                total_freed,
                total_freed as f64 / (1024.0 * 1024.0)
            ))
            .await;
    }

    let result = serde_json::json!({
        "dry_run": params.dry_run,
        "cloud": params.storage,
        "bytes_freed_local": total_freed,
        "backends": local_summary,
    });

    // Audit only when we actually deleted something. Dry-run is
    // read-only inspection; skipping the entry keeps the chain
    // focused on state changes.
    if !params.dry_run
        && let Some(audit) = state.audit.as_ref()
    {
        audit.try_append(
            "gc.run",
            AuditActor::system(),
            serde_json::json!({
                "cloud": params.storage,
                "bytes_freed_local": total_freed,
                "backends": backend_names,
            }),
            AuditResult::Ok,
        );
    }

    emitter.emit(JobEvent::result(result)).await;
    emitter.emit(JobEvent::done(0)).await;
}

/// Walk every volume's `pages.idx` and bucket the live chunk hashes
/// by `(backend, namespace)`. A volume whose manifest or page index
/// can't be read is logged and skipped — one corrupt volume must not
/// stall GC for the rest.
fn collect_live_hashes(data_dir: &Path) -> anyhow::Result<LiveSet> {
    let mut out: LiveSet = HashMap::new();
    for name in VolumeManifest::list(data_dir)? {
        let manifest = match VolumeManifest::load(data_dir, &name) {
            Ok(m) => m,
            Err(e) => {
                warn!(
                    "gc: skipping volume '{}' - manifest load failed: {}",
                    name, e
                );
                continue;
            }
        };
        let vol_dir = VolumeManifest::dir_for(data_dir, &name);
        let page_index = match PageIndex::open(
            &PageIndex::path_for(&vol_dir),
            manifest.uuid,
            u64::from(manifest.page_size_bytes),
        ) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    "gc: skipping volume '{}' - pages.idx open failed: {}",
                    name, e
                );
                continue;
            }
        };
        let bucket = out
            .entry((manifest.backend.clone(), manifest.pool_namespace()))
            .or_default();
        for record in page_index.iter() {
            match record {
                Ok((_page_id, hash)) => {
                    bucket.insert(hex::encode(hash));
                }
                Err(e) => {
                    warn!("gc: volume '{}' - pages.idx iteration failed: {}", name, e);
                    break;
                }
            }
        }
    }
    Ok(out)
}

fn run_local_gc(
    data_dir: &Path,
    backend_name: &str,
    live: &LiveSet,
    dry_run: bool,
) -> anyhow::Result<(Vec<String>, u64)> {
    let mut lines: Vec<String> = Vec::new();
    let mut bytes_freed = 0u64;
    let empty: HashSet<String> = HashSet::new();

    // Shared (Global-scope) pool.
    let shared_live = live
        .get(&(backend_name.to_string(), None))
        .unwrap_or(&empty);
    let shared_pool = ChunkPool::new(data_dir, backend_name)?;
    bytes_freed = bytes_freed.saturating_add(sweep_one_pool(
        &shared_pool,
        shared_live,
        dry_run,
        "shared pool",
        None,
        &mut lines,
    )?);

    // Local-scope per-volume namespaces: every namespace named in the
    // live set, plus any orphan namespace dir still on disk whose
    // volume was destroyed (empty live set → every chunk is orphaned).
    let pool_root = data_dir.join("chunks").join(backend_name);
    let mut namespaces: HashMap<String, &HashSet<String>> = HashMap::new();
    for ((b, ns), hashes) in live.iter() {
        if b != backend_name {
            continue;
        }
        if let Some(name) = ns {
            namespaces.insert(name.clone(), hashes);
        }
    }
    if pool_root.is_dir() {
        for entry in fs::read_dir(&pool_root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            // 2-hex dirs are the shared pool's shard dirs, not a
            // per-volume namespace.
            if name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            namespaces.entry(name).or_insert(&empty);
        }
    }

    for (ns, ns_live) in namespaces {
        let ns_pool = ChunkPool::new_namespaced(data_dir, backend_name, &ns)?;
        let context = format!("namespace '{}'", ns);
        bytes_freed = bytes_freed.saturating_add(sweep_one_pool(
            &ns_pool,
            ns_live,
            dry_run,
            &context,
            Some(&ns),
            &mut lines,
        )?);
        if !dry_run && ns_live.is_empty() {
            let _ = remove_empty_pool_dir(&ns_pool.pool_dir());
        }
    }

    Ok((lines, if dry_run { 0 } else { bytes_freed }))
}

fn sweep_one_pool(
    pool: &ChunkPool,
    live: &HashSet<String>,
    dry_run: bool,
    context: &str,
    namespace: Option<&str>,
    lines: &mut Vec<String>,
) -> anyhow::Result<u64> {
    let chunks = pool.iter_chunks()?;
    let total = chunks.len();
    let mut orphans = 0usize;
    let mut bytes_freed = 0u64;
    let ns_label = namespace.unwrap_or("(shared)");

    for (hash, size) in chunks {
        if live.contains(&hash) {
            continue;
        }
        orphans += 1;
        bytes_freed += size;
        if dry_run {
            lines.push(format!(
                "  [dry-run] would delete local chunk {}.. ({} bytes, {})",
                &hash[..hash.len().min(8)],
                size,
                ns_label,
            ));
        } else {
            pool.remove(&hash)?;
            lines.push(format!(
                "  deleted local chunk {}.. ({} bytes, {})",
                &hash[..hash.len().min(8)],
                size,
                ns_label,
            ));
        }
    }

    lines.push(format!(
        "  {}: {} total chunks, {} orphans removed",
        context, total, orphans
    ));
    Ok(if dry_run { 0 } else { bytes_freed })
}

/// Tear down a now-empty per-volume namespace directory tree. Best-
/// effort: a non-empty shard dir simply fails the `remove_dir` and is
/// left in place.
fn remove_empty_pool_dir(pool_dir: &Path) -> std::io::Result<()> {
    if !pool_dir.is_dir() {
        return Ok(());
    }
    for s1 in fs::read_dir(pool_dir)? {
        let s1 = s1?;
        if !s1.file_type()?.is_dir() {
            continue;
        }
        for s2 in fs::read_dir(s1.path())? {
            let s2 = s2?;
            if s2.file_type()?.is_dir() {
                let _ = fs::remove_dir(s2.path());
            }
        }
        let _ = fs::remove_dir(s1.path());
    }
    let _ = fs::remove_dir(pool_dir);
    Ok(())
}

async fn run_storage_gc(
    emitter: &JobEmitter,
    cfg: &ObjectStoreConfig,
    backend_name: &str,
    live: &LiveSet,
    dry_run: bool,
) -> anyhow::Result<()> {
    let backend = cfg.create_backend_named(backend_name).await?;
    let keys = backend.list_objects("chunks/").await?;
    let mut orphans = 0usize;
    let mut total = 0usize;

    let empty: HashSet<String> = HashSet::new();
    let live_for_backend: HashMap<Option<&str>, &HashSet<String>> = live
        .iter()
        .filter(|((b, _), _)| b == backend_name)
        .map(|((_, ns), hashes)| (ns.as_deref(), hashes))
        .collect();

    for key in &keys {
        let parsed = match parse_namespace_and_hash(key) {
            Some(p) => p,
            None => continue,
        };
        total += 1;
        let live_set = live_for_backend
            .get(&parsed.namespace.as_deref())
            .copied()
            .unwrap_or(&empty);
        if live_set.contains(&parsed.hash) {
            continue;
        }
        orphans += 1;
        let ns_label = parsed.namespace.as_deref().unwrap_or("(shared)");
        if dry_run {
            emitter
                .info(format!(
                    "  [dry-run] would delete cloud object {} (hash {}.., {})",
                    key,
                    &parsed.hash[..parsed.hash.len().min(8)],
                    ns_label,
                ))
                .await;
        } else {
            backend.delete_object(key).await?;
            emitter
                .info(format!(
                    "  deleted cloud object {} (hash {}.., {})",
                    key,
                    &parsed.hash[..parsed.hash.len().min(8)],
                    ns_label,
                ))
                .await;
        }
    }
    emitter
        .info(format!(
            "  Cloud bucket: {} total chunk objects, {} orphans removed",
            total, orphans
        ))
        .await;
    Ok(())
}

struct ParsedChunkKey {
    namespace: Option<String>,
    hash: String,
}

/// Parse a cloud chunk key — `chunks/[<ns>/]<aa>/<bb>/<hash>.dat` —
/// into its namespace + hash. The key shape is produced by
/// `ChunkPool::object_key_for`, shared with VTL. Anything that doesn't
/// match (other prefixes, malformed hash) → `None`.
fn parse_namespace_and_hash(key: &str) -> Option<ParsedChunkKey> {
    let stripped = key.strip_suffix(".dat")?;
    let rest = stripped.strip_prefix("chunks/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    let (namespace, hash_part) = match parts.as_slice() {
        [aa, bb, h] if is_two_hex(aa) && is_two_hex(bb) => (None, *h),
        [ns, aa, bb, h] if is_two_hex(aa) && is_two_hex(bb) && !ns.is_empty() => {
            (Some(ns.to_string()), *h)
        }
        _ => return None,
    };
    if hash_part.len() != 64 || !hash_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(ParsedChunkKey {
        namespace,
        hash: hash_part.to_string(),
    })
}

fn is_two_hex(s: &str) -> bool {
    s.len() == 2 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_block::DedupScope;
    use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
    use tempfile::TempDir;

    #[test]
    fn parse_shared_pool_key() {
        let h = "a".repeat(64);
        let key = format!("chunks/aa/bb/{}.dat", h);
        let parsed = parse_namespace_and_hash(&key).unwrap();
        assert!(parsed.namespace.is_none());
        assert_eq!(parsed.hash, h);
    }

    #[test]
    fn parse_namespaced_key() {
        let h = "a".repeat(64);
        // VSA namespaces are the 32-char volume UUID hex.
        let ns = "0123456789abcdef0123456789abcdef";
        let key = format!("chunks/{}/aa/bb/{}.dat", ns, h);
        let parsed = parse_namespace_and_hash(&key).unwrap();
        assert_eq!(parsed.namespace.as_deref(), Some(ns));
        assert_eq!(parsed.hash, h);
    }

    fn make_volume(data_dir: &Path, name: &str, backend: &str) -> VolumeManifest {
        VolumeManifest::new(
            name.to_string(),
            4 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            backend.to_string(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap()
        .create(data_dir)
        .unwrap()
    }

    /// Seal two chunks under a Local volume's namespace but map only
    /// one into `pages.idx` (overwrite page 0 to orphan the first).
    /// A non-dry-run sweep removes the orphan and keeps the live
    /// chunk; a dry-run sweep deletes nothing.
    #[test]
    fn local_gc_removes_orphan_keeps_live() {
        for dry_run in [false, true] {
            let tmp = TempDir::new().unwrap();
            let data_dir = tmp.path();
            let manifest = make_volume(data_dir, "vol-a", "primary");
            let ns = manifest.pool_namespace().unwrap();

            let pool = ChunkPool::new_namespaced(data_dir, "primary", &ns).unwrap();
            let (orphan_hash, _) = pool.insert_bytes(&[0x11; 4096]).unwrap();
            let (live_hash, _) = pool.insert_bytes(&[0x22; 2048]).unwrap();

            // Map page 0 to the orphan first, then overwrite with the
            // live chunk — the orphan is now referenced by nothing.
            let vol_dir = VolumeManifest::dir_for(data_dir, "vol-a");
            let pages = PageIndex::open(
                &PageIndex::path_for(&vol_dir),
                manifest.uuid,
                u64::from(manifest.page_size_bytes),
            )
            .unwrap();
            let orphan_bytes: [u8; 32] = hex::decode(&orphan_hash).unwrap().try_into().unwrap();
            let live_bytes: [u8; 32] = hex::decode(&live_hash).unwrap().try_into().unwrap();
            pages.set(0, &orphan_bytes).unwrap();
            pages.set(0, &live_bytes).unwrap();

            let live = collect_live_hashes(data_dir).unwrap();
            assert_eq!(
                live.get(&("primary".to_string(), Some(ns.clone()))),
                Some(&HashSet::from([live_hash.clone()])),
                "only the chunk still mapped into pages.idx is live"
            );

            let (_lines, freed) = run_local_gc(data_dir, "primary", &live, dry_run).unwrap();

            if dry_run {
                assert_eq!(freed, 0, "dry-run frees nothing");
                assert!(pool.exists(&orphan_hash), "dry-run keeps the orphan");
            } else {
                assert_eq!(freed, 4096, "non-dry-run frees the 4 KiB orphan");
                assert!(!pool.exists(&orphan_hash), "orphan chunk removed");
            }
            assert!(pool.exists(&live_hash), "live chunk must survive");
        }
    }
}
