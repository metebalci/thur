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
//! - There is no storage index-page sweep. VSA uploads only chunk
//!   objects (`chunks/[<ns>/]<aa>/<bb>/<hash>.dat`); it persists no
//!   `manifests/<...>/page-NNN` objects, so VTL's
//!   `run_storage_index_pages_gc` has no analogue here.
//!
//! The namespace for a `Local`-scope volume is its UUID hex
//! ([`VolumeManifest::pool_namespace`]); `Global`-scope volumes share
//! the per-backend pool (namespace `None`).
//!
//! Body params: `{ "dry_run": bool, "storage": bool }`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use core_block::{ChunkPool, PageIndex, SnapshotManifest, VolumeManifest};
use serde::Deserialize;
use shared_admin_server::{JobEmitter, JobEvent};
use shared_audit::{AuditActor, AuditResult};
use shared_pool::PoolBudget;
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
// Live hashes keyed on the raw 32-byte BLAKE3 hash, not its 64-char hex
// string: ~4x smaller per entry, which keeps the all-volumes-and-
// snapshots live set off the multi-GB / OOM path at scale (issue #222).
type LiveSet = HashMap<(String, Option<String>), HashSet<[u8; 32]>>;

/// `(backend, namespace)` buckets whose live set could not be read in
/// full (a page-index open/iteration error on a volume or snapshot we
/// *could* identify). Deletion is disabled for these buckets so a
/// transient read error can't be misread as orphanhood — issue #145.
/// Errors where the bucket itself can't be identified (manifest load
/// failure) abort the whole GC instead, since we can't tell which bucket
/// to protect.
type PoisonSet = HashSet<(String, Option<String>)>;

/// Recent-seal grace window for GC (issue #141): never delete a pool
/// chunk (or its storage object) sealed within this many seconds, since
/// it may be referenced by an in-flight write that landed after the
/// live-set snapshot. Generous so a chunk sealed during the whole GC
/// pass is protected; a genuine orphan is reclaimed on a later run.
const GC_RECENT_SEAL_GRACE_SECS: u64 = 3600;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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
    let collected: Result<(LiveSet, PoisonSet), anyhow::Error> =
        tokio::task::spawn_blocking(move || collect_live_hashes(&dd_for_collect))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("collect panicked: {}", e)));
    let (live, poisoned) = match collected {
        Ok(l) => l,
        Err(e) => {
            emitter
                .emit(JobEvent::done_with_error(2, format!("collect: {}", e)))
                .await;
            return;
        }
    };

    if !poisoned.is_empty() {
        emitter
            .info(format!(
                "WARNING: {} pool(s) had an unreadable live set; deletion is DISABLED for them \
                 this run (orphans there are retained, not reclaimed).",
                poisoned.len()
            ))
            .await;
        emitter.info("").await;
    }

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
        // fs::remove_file in a loop. The per-backend PoolBudget rides
        // along so each orphan removal releases its bytes back to the
        // budget (keeping `current_bytes()` exact for the eviction
        // worker, which sources its per-tick usage from the budget).
        let bn = backend_name.clone();
        let live_clone = live.clone();
        let dd = data_dir.clone();
        let dry = params.dry_run;
        let budget = state.pool_budgets.get(backend_name).cloned();
        let poisoned_clone = poisoned.clone();
        let lines_with_freed = tokio::task::spawn_blocking(move || {
            run_local_gc(&dd, &bn, &live_clone, &poisoned_clone, dry, budget.as_deref())
        })
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
                &state,
                backend_name,
                &live,
                &poisoned,
                params.dry_run,
            )
            .await
        {
            emitter
                .error(format!("storage gc on backend {}: {}", backend_name, e))
                .await;
        }
        emitter.info("").await;
    }
    if !params.storage {
        emitter
            .info("(Skipping storage GC — re-run with storage:true to clean buckets too.)")
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
        "storage": params.storage,
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
                "storage": params.storage,
                "bytes_freed_local": total_freed,
                "backends": backend_names,
            }),
            AuditResult::Ok,
        );
    }

    emitter.emit(JobEvent::result(result)).await;
    emitter.emit(JobEvent::done(0)).await;
}

/// Walk every volume's `pages.idx` **and every snapshot's frozen
/// `pages.idx`**, bucketing the live chunk hashes by
/// `(backend, namespace)`. A volume or snapshot whose manifest or page
/// index can't be read is logged and skipped — one corrupt member must
/// not stall GC for the rest.
///
/// Snapshots (issue #13) are what make copy-on-write reclaimable: a
/// snapshot's frozen index keeps the parent's pre-overwrite chunks in
/// the live set, keyed on the *family* namespace
/// ([`SnapshotManifest::pool_namespace`]) so its hashes union with the
/// parent's and any clones' into one bucket. The union is a `HashSet`,
/// so a chunk shared across family members is counted once and only
/// reclaimed when no member references it.
fn collect_live_hashes(data_dir: &Path) -> anyhow::Result<(LiveSet, PoisonSet)> {
    let mut out: LiveSet = HashMap::new();
    let mut poisoned: PoisonSet = HashSet::new();
    for name in VolumeManifest::list(data_dir)? {
        // A manifest we can't load means we can't identify the volume's
        // (backend, namespace) bucket — and run_local_gc would otherwise
        // treat its on-disk namespace dir as a fully-orphan destroyed
        // volume and delete every chunk. We can't protect a bucket we
        // can't name, so abort the whole GC rather than risk it (#145).
        let manifest = VolumeManifest::load(data_dir, &name).map_err(|e| {
            anyhow::anyhow!(
                "gc aborted: volume '{}' manifest load failed ({}); refusing to GC with an \
                 incomplete live set",
                name,
                e
            )
        })?;
        let key = (manifest.backend.clone(), manifest.pool_namespace());
        let vol_dir = VolumeManifest::dir_for(data_dir, &name);
        let page_index = match PageIndex::open(
            &PageIndex::path_for(&vol_dir),
            manifest.uuid,
            u64::from(manifest.page_size_bytes),
        ) {
            Ok(p) => p,
            Err(e) => {
                // Bucket is known: protect it instead of dropping it.
                warn!(
                    "gc: volume '{}' pages.idx open failed ({}); disabling deletion for its pool",
                    name, e
                );
                poisoned.insert(key.clone());
                out.entry(key).or_default();
                continue;
            }
        };
        let bucket = out.entry(key.clone()).or_default();
        for record in page_index.iter() {
            match record {
                Ok((_page_id, hash)) => {
                    // Raw 32-byte hash, not its hex string (issue #222).
                    bucket.insert(hash);
                }
                Err(e) => {
                    warn!(
                        "gc: volume '{}' pages.idx iteration failed ({}); disabling deletion \
                         for its pool",
                        name, e
                    );
                    poisoned.insert(key);
                    break;
                }
            }
        }
    }

    // Second pass — every snapshot's frozen page index. Same bucketing,
    // keyed on the snapshot's family namespace so it unions with its
    // parent/clones.
    for (parent, snap) in SnapshotManifest::list_all(data_dir)? {
        let manifest = SnapshotManifest::load(data_dir, &parent, &snap).map_err(|e| {
            anyhow::anyhow!(
                "gc aborted: snapshot '{}/{}' manifest load failed ({}); refusing to GC with an \
                 incomplete live set",
                parent,
                snap,
                e
            )
        })?;
        let key = (manifest.backend.clone(), manifest.pool_namespace());
        let idx_path = SnapshotManifest::page_index_path(data_dir, &parent, &snap);
        let page_index = match PageIndex::open(
            &idx_path,
            manifest.uuid,
            u64::from(manifest.page_size_bytes),
        ) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    "gc: snapshot '{}/{}' pages.idx open failed ({}); disabling deletion for its \
                     pool",
                    parent, snap, e
                );
                poisoned.insert(key.clone());
                out.entry(key).or_default();
                continue;
            }
        };
        let bucket = out.entry(key.clone()).or_default();
        for record in page_index.iter() {
            match record {
                Ok((_page_id, hash)) => {
                    // Raw 32-byte hash, not its hex string (issue #222).
                    bucket.insert(hash);
                }
                Err(e) => {
                    warn!(
                        "gc: snapshot '{}/{}' pages.idx iteration failed ({}); disabling deletion \
                         for its pool",
                        parent, snap, e
                    );
                    poisoned.insert(key);
                    break;
                }
            }
        }
    }
    Ok((out, poisoned))
}

fn run_local_gc(
    data_dir: &Path,
    backend_name: &str,
    live: &LiveSet,
    poisoned: &PoisonSet,
    dry_run: bool,
    budget: Option<&PoolBudget>,
) -> anyhow::Result<(Vec<String>, u64)> {
    let mut lines: Vec<String> = Vec::new();
    let mut bytes_freed = 0u64;
    let empty: HashSet<[u8; 32]> = HashSet::new();

    // Shared (Global-scope) pool. Skip entirely if its live set was
    // incomplete this run (#145).
    if poisoned.contains(&(backend_name.to_string(), None)) {
        lines.push("  shared pool: skipped (live set unreadable this run)".to_string());
    } else {
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
            budget,
            &mut lines,
        )?);
    }

    // Local-scope per-volume namespaces: every namespace named in the
    // live set, plus any orphan namespace dir still on disk whose
    // volume was destroyed (empty live set → every chunk is orphaned).
    let pool_root = data_dir.join("chunks").join(backend_name);
    let mut namespaces: HashMap<String, &HashSet<[u8; 32]>> = HashMap::new();
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
        // Skip namespaces whose live set was incomplete this run (#145):
        // an unreadable index must not let the sweep mistake the volume's
        // chunks for orphans.
        if poisoned.contains(&(backend_name.to_string(), Some(ns.clone()))) {
            lines.push(format!(
                "  namespace '{}': skipped (live set unreadable this run)",
                ns
            ));
            continue;
        }
        let ns_pool = ChunkPool::new_namespaced(data_dir, backend_name, &ns)?;
        let context = format!("namespace '{}'", ns);
        bytes_freed = bytes_freed.saturating_add(sweep_one_pool(
            &ns_pool,
            ns_live,
            dry_run,
            &context,
            Some(&ns),
            budget,
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
    live: &HashSet<[u8; 32]>,
    dry_run: bool,
    context: &str,
    namespace: Option<&str>,
    budget: Option<&PoolBudget>,
    lines: &mut Vec<String>,
) -> anyhow::Result<u64> {
    let mut total = 0usize;
    let mut orphans = 0usize;
    let mut bytes_freed = 0u64;
    let ns_label = namespace.unwrap_or("(shared)");
    let recent_cutoff = now_secs().saturating_sub(GC_RECENT_SEAL_GRACE_SECS);

    // Stream the pool (no whole-pool `Vec<(String, u64)>`; issue #222).
    // The live-set membership test is a raw 32-byte compare; the hex
    // string the pool ops (`is_pinned` / `chunk_mtime_secs` / `remove` /
    // log lines) need is materialized only for orphan *candidates*, a
    // small fraction. A `remove` failure is captured and surfaced after
    // the walk (the infallible callback can't `?`).
    let mut remove_err: Option<anyhow::Error> = None;
    pool.for_each_chunk(|hash, size| {
        if remove_err.is_some() {
            return;
        }
        total += 1;
        if live.contains(&hash) {
            return;
        }
        let hex = hex::encode(hash);
        if pool.is_pinned(&hex) {
            lines.push(format!(
                "  skipped orphan chunk {}.. ({} bytes, {}) - pinned by outstanding ROD token",
                &hex[..hex.len().min(8)],
                size,
                ns_label,
            ));
            return;
        }
        // Recent-seal grace window (issue #141): a chunk sealed after the
        // phase-1 live-set snapshot looks like an orphan but is referenced
        // by an in-flight WRITE GC hasn't observed; deleting it loses
        // host-acked data before its upload completes. A genuine orphan
        // ages past the window and is reclaimed on a later run.
        if let Some(mtime) = pool.chunk_mtime_secs(&hex)
            && mtime >= recent_cutoff
        {
            lines.push(format!(
                "  skipped recent chunk {}.. ({} bytes, {}) - sealed within grace window",
                &hex[..hex.len().min(8)],
                size,
                ns_label,
            ));
            return;
        }
        orphans += 1;
        bytes_freed += size;
        if dry_run {
            lines.push(format!(
                "  [dry-run] would delete local chunk {}.. ({} bytes, {})",
                &hex[..hex.len().min(8)],
                size,
                ns_label,
            ));
        } else if let Err(e) = pool.remove(&hex) {
            remove_err = Some(e.into());
        } else {
            // Release the freed bytes back to the per-backend budget so
            // `current_bytes()` stays equal to on-disk pool bytes (the
            // eviction worker reads the budget instead of rescanning).
            // Same `(size, namespace)` pairing the eviction path uses.
            if let Some(b) = budget {
                b.release(size, namespace);
            }
            lines.push(format!(
                "  deleted local chunk {}.. ({} bytes, {})",
                &hex[..hex.len().min(8)],
                size,
                ns_label,
            ));
        }
    })?;
    if let Some(e) = remove_err {
        return Err(e);
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
    state: &AdminState,
    backend_name: &str,
    live: &LiveSet,
    poisoned: &PoisonSet,
    dry_run: bool,
) -> anyhow::Result<()> {
    // Use the daemon's cached backend instance, not a fresh one: every
    // backend is wrapped in CachingObjectStoreBackend, and delete_object
    // only invalidates the instance it runs on. Deleting through a
    // private instance would leave the long-lived instance (used by the
    // upload worker / read refetch) still asserting the key is present —
    // a later identical-content write would then skip its PUT and lose
    // data (#146).
    let backend = crate::admin::handlers::get_or_init_backend(state, backend_name).await?;
    let keys = backend.list_objects("chunks/").await?;
    let mut orphans = 0usize;
    let mut total = 0usize;
    let recent_cutoff = now_secs().saturating_sub(GC_RECENT_SEAL_GRACE_SECS);

    let empty: HashSet<[u8; 32]> = HashSet::new();
    let live_for_backend: HashMap<Option<&str>, &HashSet<[u8; 32]>> = live
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
        // Never delete from a bucket whose live set was incomplete this
        // run (#145).
        if poisoned.contains(&(backend_name.to_string(), parsed.namespace.clone())) {
            continue;
        }
        let live_set = live_for_backend
            .get(&parsed.namespace.as_deref())
            .copied()
            .unwrap_or(&empty);
        // The live set is keyed on the raw 32-byte hash (issue #222);
        // decode the object key's hex. A key that doesn't decode can't
        // be matched to a live chunk and is left in place (never deleted).
        let Some(hash_bytes) = shared_pool::decode_hash_hex(&parsed.hash) else {
            continue;
        };
        if live_set.contains(&hash_bytes) {
            continue;
        }
        let ns_label = parsed.namespace.as_deref().unwrap_or("(shared)");
        // Recent-seal grace (issue #141): a chunk uploaded after the
        // live-set snapshot looks orphaned but is referenced by a write
        // GC hasn't observed. Its local pool file was sealed just as
        // recently, so skip the storage delete when the local file is
        // present and within the grace window — protecting the DR copy.
        let local_pool = match parsed.namespace.as_deref() {
            Some(ns) => ChunkPool::new_namespaced(&state.data_dir, backend_name, ns),
            None => ChunkPool::new(&state.data_dir, backend_name),
        };
        if let Ok(pool) = local_pool
            && let Some(mtime) = pool.chunk_mtime_secs(&parsed.hash)
            && mtime >= recent_cutoff
        {
            emitter
                .info(format!(
                    "  skipped recent storage object {} (hash {}.., {}) - sealed within grace window",
                    key,
                    &parsed.hash[..parsed.hash.len().min(8)],
                    ns_label,
                ))
                .await;
            continue;
        }
        orphans += 1;
        if dry_run {
            emitter
                .info(format!(
                    "  [dry-run] would delete storage object {} (hash {}.., {})",
                    key,
                    &parsed.hash[..parsed.hash.len().min(8)],
                    ns_label,
                ))
                .await;
        } else {
            backend.delete_object(key).await?;
            emitter
                .info(format!(
                    "  deleted storage object {} (hash {}.., {})",
                    key,
                    &parsed.hash[..parsed.hash.len().min(8)],
                    ns_label,
                ))
                .await;
        }
    }
    emitter
        .info(format!(
            "  Storage bucket: {} total chunk objects, {} orphans removed",
            total, orphans
        ))
        .await;
    Ok(())
}

struct ParsedChunkKey {
    namespace: Option<String>,
    hash: String,
}

/// Parse a storage chunk key — `chunks/[<ns>/]<aa>/<bb>/<hash>.dat` —
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

    /// Age every pool chunk file under `data_dir/chunks` past the GC
    /// recent-seal grace window so the deletion-path tests aren't masked
    /// by it (issue #141 grace skips freshly-sealed chunks). Production
    /// keeps the grace; this only backdates mtimes for the test.
    fn backdate_chunks(data_dir: &Path) {
        fn walk(dir: &Path) {
            if let Ok(rd) = fs::read_dir(dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        walk(&p);
                    } else if p.extension().is_some_and(|x| x == "dat")
                        && let Ok(f) = fs::OpenOptions::new().write(true).open(&p)
                    {
                        let old =
                            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000);
                        let _ = f.set_modified(old);
                    }
                }
            }
        }
        walk(&data_dir.join("chunks"));
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

            let (live, poisoned) = collect_live_hashes(data_dir).unwrap();
            assert!(poisoned.is_empty());
            assert_eq!(
                live.get(&("primary".to_string(), Some(ns.clone()))),
                Some(&HashSet::from([live_bytes])),
                "only the chunk still mapped into pages.idx is live"
            );

            backdate_chunks(data_dir);
            let (_lines, freed) =
                run_local_gc(data_dir, "primary", &live, &poisoned, dry_run, None).unwrap();

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

    /// Budget exactness: a non-dry-run sweep must release exactly the
    /// orphan bytes back to the per-backend `PoolBudget` (so the
    /// eviction worker, which reads `current_bytes()` instead of
    /// rescanning, sees the reclaimed space). The live chunk's bytes
    /// stay reserved; a dry-run sweep leaves the budget untouched.
    #[test]
    fn local_gc_releases_orphan_bytes_to_budget() {
        for dry_run in [false, true] {
            let tmp = TempDir::new().unwrap();
            let data_dir = tmp.path();
            let manifest = make_volume(data_dir, "vol-a", "primary");
            let ns = manifest.pool_namespace().unwrap();

            let pool = ChunkPool::new_namespaced(data_dir, "primary", &ns).unwrap();
            let (orphan_hash, _) = pool.insert_bytes(&[0x11; 4096]).unwrap();
            let (live_hash, _) = pool.insert_bytes(&[0x22; 2048]).unwrap();

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

            // Seed the budget to mirror the on-disk reality both chunks
            // create under the volume's namespace.
            let budget = PoolBudget::new(data_dir.to_path_buf(), 0, 0, 80);
            budget.force_reserve(4096, Some(&ns));
            budget.force_reserve(2048, Some(&ns));
            assert_eq!(budget.current_bytes(), 4096 + 2048);

            let (live, poisoned) = collect_live_hashes(data_dir).unwrap();
            backdate_chunks(data_dir);
            run_local_gc(data_dir, "primary", &live, &poisoned, dry_run, Some(&budget)).unwrap();

            if dry_run {
                assert_eq!(
                    budget.current_bytes(),
                    4096 + 2048,
                    "dry-run must not release any budget"
                );
            } else {
                assert_eq!(
                    budget.current_bytes(),
                    2048,
                    "only the live chunk's bytes remain reserved after GC"
                );
            }
        }
    }

    /// Issue #141: a chunk sealed within the grace window (i.e. possibly
    /// referenced by an in-flight write not yet in the live set) must NOT
    /// be deleted even though it's absent from the live set.
    #[test]
    fn recent_orphan_chunk_survives_grace_window() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();
        let manifest = make_volume(data_dir, "vol-a", "primary");
        let ns = manifest.pool_namespace().unwrap();
        let pool = ChunkPool::new_namespaced(data_dir, "primary", &ns).unwrap();
        // A just-sealed chunk referenced by nothing in the (empty) live
        // set — but its mtime is now, inside the grace window.
        let (hash, _) = pool.insert_bytes(&[0x44; 4096]).unwrap();

        let live = LiveSet::new();
        let (_lines, freed) =
            run_local_gc(data_dir, "primary", &live, &PoisonSet::new(), false, None).unwrap();
        assert_eq!(freed, 0, "recently-sealed chunk must not be reclaimed");
        assert!(
            pool.exists(&hash),
            "recently-sealed chunk survives the grace window"
        );
    }

    /// A poisoned `(backend, namespace)` bucket must not be swept even
    /// though its live set looks empty — the protective half of #145.
    #[test]
    fn poisoned_namespace_is_not_swept() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();
        let manifest = make_volume(data_dir, "vol-a", "primary");
        let ns = manifest.pool_namespace().unwrap();

        let pool = ChunkPool::new_namespaced(data_dir, "primary", &ns).unwrap();
        let (h, _) = pool.insert_bytes(&[0x33; 4096]).unwrap();

        // Empty live set (as if the index couldn't be read) but the
        // bucket is poisoned: the sweep must protect it.
        let live = LiveSet::new();
        let mut poisoned = PoisonSet::new();
        poisoned.insert(("primary".to_string(), Some(ns.clone())));

        let (_lines, freed) =
            run_local_gc(data_dir, "primary", &live, &poisoned, false, None).unwrap();
        assert_eq!(freed, 0, "poisoned namespace must free nothing");
        assert!(pool.exists(&h), "poisoned namespace chunk must survive GC");
    }

    /// An unreadable volume manifest must abort the whole GC (we can't
    /// identify the bucket to protect), not silently shrink the live set
    /// and delete the volume's chunks — the abort half of #145.
    #[test]
    fn collect_aborts_on_unreadable_manifest() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();
        make_volume(data_dir, "vol-a", "primary");
        // Corrupt the manifest so load() fails.
        let manifest_path = VolumeManifest::path_for(data_dir, "vol-a");
        fs::write(&manifest_path, b"{ this is not valid manifest json").unwrap();

        let res = collect_live_hashes(data_dir);
        assert!(
            res.is_err(),
            "an unreadable manifest must abort GC rather than drop the volume from the live set"
        );
    }

    /// Take a snapshot the way the daemon does: copy the live pages.idx
    /// into the snapshot dir and persist the snapshot manifest.
    fn take_snapshot(data_dir: &Path, parent: &VolumeManifest, snap: &str) {
        let snap_dir = SnapshotManifest::dir_for(data_dir, &parent.name, snap);
        fs::create_dir_all(&snap_dir).unwrap();
        let src = PageIndex::path_for(&VolumeManifest::dir_for(data_dir, &parent.name));
        fs::copy(&src, PageIndex::path_for(&snap_dir)).unwrap();
        SnapshotManifest::new(snap.to_string(), parent, parent.size_bytes)
            .unwrap()
            .persist(&snap_dir)
            .unwrap();
    }

    /// The load-bearing copy-on-write property: a chunk the parent has
    /// overwritten is retained while a snapshot still references it, and
    /// reclaimed once the snapshot is destroyed.
    #[test]
    fn snapshot_retains_overwritten_chunk_then_reclaims() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();
        let manifest = make_volume(data_dir, "vol-a", "primary");
        let ns = manifest.pool_namespace().unwrap();

        let pool = ChunkPool::new_namespaced(data_dir, "primary", &ns).unwrap();
        let (old_hash, _) = pool.insert_bytes(&[0xAA; 4096]).unwrap();
        let (new_hash, _) = pool.insert_bytes(&[0xBB; 4096]).unwrap();

        let vol_dir = VolumeManifest::dir_for(data_dir, "vol-a");
        let pages = PageIndex::open(
            &PageIndex::path_for(&vol_dir),
            manifest.uuid,
            u64::from(manifest.page_size_bytes),
        )
        .unwrap();
        let old_bytes: [u8; 32] = hex::decode(&old_hash).unwrap().try_into().unwrap();
        let new_bytes: [u8; 32] = hex::decode(&new_hash).unwrap().try_into().unwrap();

        // Page 0 = old chunk, then snapshot freezes that mapping.
        pages.set(0, &old_bytes).unwrap();
        take_snapshot(data_dir, &manifest, "snap1");

        // Parent overwrites page 0 with the new chunk. Old chunk is now
        // orphaned from the PARENT but still held by the snapshot.
        pages.set(0, &new_bytes).unwrap();

        // Live set holds BOTH — the snapshot keeps the old hash alive.
        let (live, _poisoned) = collect_live_hashes(data_dir).unwrap();
        let bucket = live
            .get(&("primary".to_string(), Some(ns.clone())))
            .expect("namespace bucket present");
        assert!(
            bucket.contains(&old_bytes),
            "snapshot keeps overwritten chunk"
        );
        assert!(bucket.contains(&new_bytes), "parent's current chunk is live");

        // GC while the snapshot exists: nothing reclaimed.
        run_local_gc(data_dir, "primary", &live, &PoisonSet::new(), false, None).unwrap();
        assert!(pool.exists(&old_hash), "snapshot-held chunk survives GC");
        assert!(pool.exists(&new_hash));

        // Destroy the snapshot, re-collect, GC again: old chunk is now a
        // true orphan and gets reclaimed; the parent's chunk stays.
        fs::remove_dir_all(SnapshotManifest::dir_for(data_dir, "vol-a", "snap1")).unwrap();
        let (live, poisoned) = collect_live_hashes(data_dir).unwrap();
        backdate_chunks(data_dir);
        let (_lines, freed) =
            run_local_gc(data_dir, "primary", &live, &poisoned, false, None).unwrap();
        assert_eq!(freed, 4096, "the orphaned old chunk is reclaimed");
        assert!(
            !pool.exists(&old_hash),
            "old chunk gone after snapshot destroy"
        );
        assert!(pool.exists(&new_hash), "parent's chunk still live");
    }
}
