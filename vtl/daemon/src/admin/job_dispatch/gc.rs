// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `system.gc` job — orphan-chunk garbage collection for the
//! content-addressed store.
//!
//! Walker logic mirrors what used to live in
//! `vtl/cli/src/commands/gc.rs`. Moved into the daemon so the
//! sole owner of the on-disk pool can run GC without the operator
//! stopping the daemon — `chunks.idx` mutations and pool sweeps
//! serialize against each other through the daemon's existing locks.
//!
//! Body params: `{ "dry_run": bool, "cloud": bool }`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use crate::state::DaemonState;
use core_mediachanger::chunk_index::ChunkIndexFile;
use core_mediachanger::{ChunkStore, CloudBackend};
use serde::Deserialize;
use shared_admin_server::{JobEmitter, JobEvent};

#[derive(Debug, Default, Deserialize)]
pub struct GcParams {
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub cloud: bool,
}

/// Per-(backend, namespace) live-hash bucket.
type LiveSet = HashMap<(String, Option<String>), HashSet<String>>;
/// Per-(backend, barcode) live page-count bucket.
type LiveIndexPages = HashMap<(String, String), HashMap<String, u32>>;

pub async fn run(emitter: JobEmitter, body: serde_json::Value, state: Arc<DaemonState>) {
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

    // Phase 1 — collect live sets (sync, on blocking pool).
    let dd_for_collect = data_dir.clone();
    let live_collect: Result<(LiveSet, LiveIndexPages, Vec<String>), anyhow::Error> =
        tokio::task::spawn_blocking(move || {
            let live = collect_live_hashes(&dd_for_collect)?;
            let pages = collect_live_index_pages(&dd_for_collect)?;
            Ok((live, pages, Vec::new()))
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("collect panicked: {}", e)));
    let (live, live_pages, _warnings) = match live_collect {
        Ok(t) => t,
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
            "Live hashes referenced by manifests: {} across {} backend(s) ({} namespace(s))",
            total_live,
            backend_count.len(),
            live.len(),
        ))
        .await;
    emitter.info("").await;

    // Phase 2 — per-backend sweeps.
    let mut total_freed: u64 = 0;
    let mut local_summary: Vec<serde_json::Value> = Vec::new();
    let backend_names = state.cloud_config.backend_names();
    for backend_name in &backend_names {
        emitter
            .info(format!("=== Backend: {} ===", backend_name))
            .await;

        // Local pool sweep on the blocking pool — chunk-store removals
        // hit fs::remove_file in a loop.
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

        if params.cloud {
            if let Err(e) = run_cloud_gc(
                &emitter,
                &state.cloud_config,
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
            if let Err(e) = run_cloud_index_pages_gc(
                &emitter,
                &state.cloud_config,
                backend_name,
                &live_pages,
                params.dry_run,
            )
            .await
            {
                emitter
                    .error(format!(
                        "cloud index-page gc on backend {}: {}",
                        backend_name, e
                    ))
                    .await;
            }
        }
        emitter.info("").await;
    }
    if !params.cloud {
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
        "cloud": params.cloud,
        "bytes_freed_local": total_freed,
        "backends": local_summary,
    });

    // Audit only when we actually deleted something. Dry-run is
    // read-only inspection; skipping the entry keeps the chain
    // focused on state changes.
    if !params.dry_run
        && let Some(log) = state.audit_log.as_ref()
    {
        let actor = core_mediachanger::AuditActor::cli("daemon".to_string());
        log.try_append(
            "gc.run",
            actor,
            serde_json::json!({
                "cloud": params.cloud,
                "bytes_freed_local": total_freed,
                "backends": backend_names,
            }),
            core_mediachanger::AuditResult::Ok,
        );
    }

    emitter.emit(JobEvent::result(result)).await;
    emitter.emit(JobEvent::done(0)).await;
}

fn collect_live_hashes(data_dir: &Path) -> anyhow::Result<LiveSet> {
    let mut out: LiveSet = HashMap::new();
    let tapes_dir = data_dir.join("tapes");
    if !tapes_dir.is_dir() {
        return Ok(out);
    }
    for entry in fs::read_dir(&tapes_dir)? {
        let entry = entry?;
        let tape_path = entry.path();
        let manifest_path = tape_path.join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        let json = fs::read_to_string(&manifest_path)?;
        let v: serde_json::Value = serde_json::from_str(&json)?;
        let backend = match v.get("backend").and_then(|s| s.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let label = match tape_path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let namespace: Option<String> = match v.get("dedup").and_then(|d| d.as_str()) {
            Some("local") => Some(label.clone()),
            _ => None,
        };
        let chunks_idx_path = ChunkIndexFile::path_for(&tape_path);
        if !chunks_idx_path.is_file() {
            continue;
        }
        let cif = match ChunkIndexFile::open_or_create(&tape_path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let n = cif.next_id();
        let bucket = out.entry((backend, namespace)).or_default();
        for id in 0..n {
            if let Ok(rec) = cif.read(id)
                && let Some(h) = rec.hash
            {
                bucket.insert(h);
            }
        }
    }
    Ok(out)
}

fn collect_live_index_pages(data_dir: &Path) -> anyhow::Result<LiveIndexPages> {
    let mut out: LiveIndexPages = HashMap::new();
    let tapes_dir = data_dir.join("tapes");
    if !tapes_dir.is_dir() {
        return Ok(out);
    }
    for entry in fs::read_dir(&tapes_dir)? {
        let entry = entry?;
        let manifest_path = entry.path().join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        let json = fs::read_to_string(&manifest_path)?;
        let v: serde_json::Value = serde_json::from_str(&json)?;
        let backend = match v.get("backend").and_then(|s| s.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let barcode = match v.get("label").and_then(|s| s.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let epoch_obj = match v.get("index_epoch").and_then(|m| m.as_object()) {
            Some(o) => o,
            None => continue,
        };
        let mut labels: HashMap<String, u32> = HashMap::new();
        for (label, eob) in epoch_obj {
            if let Some(pages) = eob.get("pages").and_then(|p| p.as_u64())
                && pages <= u32::MAX as u64
            {
                labels.insert(label.clone(), pages as u32);
            }
        }
        out.insert((backend, barcode), labels);
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

    let shared_live = live
        .get(&(backend_name.to_string(), None))
        .unwrap_or(&empty);
    let store = ChunkStore::new(data_dir, backend_name)?;
    bytes_freed = bytes_freed.saturating_add(sweep_one_pool(
        &store,
        shared_live,
        dry_run,
        "shared pool",
        None,
        &mut lines,
    )?);

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
            if name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            namespaces.entry(name).or_insert(&empty);
        }
    }

    for (label, ns_live) in namespaces {
        let ns_store = ChunkStore::new_namespaced(data_dir, backend_name, &label)?;
        let context = format!("namespace '{}'", label);
        bytes_freed = bytes_freed.saturating_add(sweep_one_pool(
            &ns_store,
            ns_live,
            dry_run,
            &context,
            Some(&label),
            &mut lines,
        )?);
        if !dry_run && ns_live.is_empty() {
            let _ = remove_empty_pool_dir(&ns_store.pool_dir());
        }
    }

    Ok((lines, if dry_run { 0 } else { bytes_freed }))
}

fn sweep_one_pool(
    store: &ChunkStore,
    live: &HashSet<String>,
    dry_run: bool,
    context: &str,
    namespace: Option<&str>,
    lines: &mut Vec<String>,
) -> anyhow::Result<u64> {
    let pool = store.iter_chunks()?;
    let total = pool.len();
    let mut orphans = 0usize;
    let mut bytes_freed = 0u64;
    let ns_label = namespace.unwrap_or("(shared)");

    for (hash, size) in pool {
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
            store.remove(&hash)?;
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

async fn run_cloud_gc(
    emitter: &JobEmitter,
    cfg: &core_mediachanger::CloudConfig,
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
        total += 1;
        let parsed = match parse_namespace_and_hash(key) {
            Some(p) => p,
            None => continue,
        };
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

async fn run_cloud_index_pages_gc(
    emitter: &JobEmitter,
    cfg: &core_mediachanger::CloudConfig,
    backend_name: &str,
    live_pages: &LiveIndexPages,
    dry_run: bool,
) -> anyhow::Result<()> {
    let scoped: Vec<(&String, &HashMap<String, u32>)> = live_pages
        .iter()
        .filter_map(|((b, barcode), labels)| (b == backend_name).then_some((barcode, labels)))
        .collect();
    if scoped.is_empty() {
        return Ok(());
    }
    let backend = cfg.create_backend_named(backend_name).await?;
    let (total, orphans) = sweep_index_pages(emitter, &*backend, &scoped, dry_run).await?;
    emitter
        .info(format!(
            "  Cloud index pages: {} total page objects scanned, {} orphans removed",
            total, orphans
        ))
        .await;
    Ok(())
}

async fn sweep_index_pages(
    emitter: &JobEmitter,
    backend: &dyn CloudBackend,
    scoped: &[(&String, &HashMap<String, u32>)],
    dry_run: bool,
) -> anyhow::Result<(usize, usize)> {
    let mut total = 0usize;
    let mut orphans = 0usize;
    for (barcode, labels) in scoped {
        let prefix = format!("manifests/{}/", barcode);
        let keys = backend.list_objects(&prefix).await?;
        for key in &keys {
            let parsed = match parse_index_page_key(key) {
                Some(p) => p,
                None => continue,
            };
            if parsed.barcode != **barcode {
                continue;
            }
            total += 1;
            let live_count = labels.get(&parsed.label).copied().unwrap_or(0);
            if parsed.page < live_count {
                continue;
            }
            orphans += 1;
            if dry_run {
                emitter
                    .info(format!(
                        "  [dry-run] would delete index page {} (barcode {}, label {}, page {} >= live {})",
                        key, barcode, parsed.label, parsed.page, live_count,
                    ))
                    .await;
            } else {
                backend.delete_object(key).await?;
                emitter
                    .info(format!(
                        "  deleted index page {} (barcode {}, label {}, page {} >= live {})",
                        key, barcode, parsed.label, parsed.page, live_count,
                    ))
                    .await;
            }
        }
    }
    Ok((total, orphans))
}

struct ParsedIndexPageKey {
    barcode: String,
    label: String,
    page: u32,
}

fn parse_index_page_key(key: &str) -> Option<ParsedIndexPageKey> {
    let stripped = key.strip_suffix(".dat")?;
    let rest = stripped.strip_prefix("manifests/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() != 3 {
        return None;
    }
    let barcode = parts[0];
    let label = parts[1];
    let page_part = parts[2].strip_prefix("page-")?;
    if barcode.is_empty() || label.is_empty() {
        return None;
    }
    if label.contains('/') {
        return None;
    }
    let page: u32 = page_part.parse().ok()?;
    Some(ParsedIndexPageKey {
        barcode: barcode.to_string(),
        label: label.to_string(),
        page,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let key = format!("chunks/TAPE001/aa/bb/{}.dat", h);
        let parsed = parse_namespace_and_hash(&key).unwrap();
        assert_eq!(parsed.namespace.as_deref(), Some("TAPE001"));
        assert_eq!(parsed.hash, h);
    }

    #[test]
    fn parse_index_page_key_chunks() {
        let p = parse_index_page_key("manifests/TAPE001/chunks/page-000042.dat").unwrap();
        assert_eq!(p.barcode, "TAPE001");
        assert_eq!(p.label, "chunks");
        assert_eq!(p.page, 42);
    }

    #[test]
    fn parse_index_page_key_rejects_manifest_backups() {
        assert!(parse_index_page_key("manifests/TAPE001/manifest-latest.json").is_none());
    }

    #[test]
    fn gc_params_default_is_all_false() {
        let p = GcParams::default();
        assert!(!p.dry_run);
        assert!(!p.cloud);
    }

    #[test]
    fn gc_params_empty_json_uses_defaults() {
        let p: GcParams = serde_json::from_value(serde_json::json!({})).expect("empty body");
        assert!(!p.dry_run);
        assert!(!p.cloud);
    }

    #[test]
    fn gc_params_parses_explicit_flags() {
        let p: GcParams =
            serde_json::from_value(serde_json::json!({"dry_run": true, "cloud": true}))
                .expect("explicit body");
        assert!(p.dry_run);
        assert!(p.cloud);
    }

    #[test]
    fn gc_params_rejects_wrong_type() {
        assert!(serde_json::from_value::<GcParams>(serde_json::json!({"dry_run": "yes"})).is_err());
    }

    #[test]
    fn is_two_hex_accepts_only_two_hex_chars() {
        assert!(is_two_hex("aa"));
        assert!(is_two_hex("0f"));
        assert!(is_two_hex("FF"));
        assert!(!is_two_hex("a"));
        assert!(!is_two_hex("abc"));
        assert!(!is_two_hex("zz"));
    }

    #[test]
    fn collect_live_hashes_empty_when_no_tapes_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = collect_live_hashes(dir.path()).expect("collect on bare dir");
        assert!(live.is_empty());
    }

    #[test]
    fn collect_live_hashes_skips_tape_without_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A tape dir with no manifest.json is silently skipped.
        std::fs::create_dir_all(dir.path().join("tapes").join("TAPE001")).expect("mkdir tape");
        let live = collect_live_hashes(dir.path()).expect("collect");
        assert!(live.is_empty());
    }

    #[test]
    fn collect_live_index_pages_empty_when_no_tapes_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pages = collect_live_index_pages(dir.path()).expect("collect index pages");
        assert!(pages.is_empty());
    }

    #[test]
    fn collect_live_index_pages_skips_tape_without_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("tapes").join("TAPE001")).expect("mkdir tape");
        let pages = collect_live_index_pages(dir.path()).expect("collect");
        assert!(pages.is_empty());
    }

    #[test]
    fn collect_live_index_pages_reads_index_epoch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tape = dir.path().join("tapes").join("TAPE001");
        std::fs::create_dir_all(&tape).expect("mkdir tape");
        std::fs::write(
            tape.join("manifest.json"),
            r#"{"backend":"s3b","label":"TAPE001","index_epoch":{"chunks":{"pages":4}}}"#,
        )
        .expect("write manifest");
        let pages = collect_live_index_pages(dir.path()).expect("collect");
        let entry = pages
            .get(&("s3b".to_string(), "TAPE001".to_string()))
            .expect("entry present");
        assert_eq!(entry.get("chunks").copied(), Some(4));
    }

    #[test]
    fn remove_empty_pool_dir_noop_on_missing_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        remove_empty_pool_dir(&dir.path().join("absent")).expect("noop");
    }

    #[test]
    fn remove_empty_pool_dir_clears_empty_two_level_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = dir.path().join("pool");
        std::fs::create_dir_all(pool.join("aa").join("bb")).expect("mkdir tree");
        remove_empty_pool_dir(&pool).expect("remove empty tree");
        assert!(!pool.exists());
    }

    #[test]
    fn sweep_one_pool_deletes_orphans_keeps_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ChunkStore::new(dir.path(), "s3b").expect("chunk store");
        let (live_hash, _) = store.insert_bytes(b"keep me").expect("insert live");
        let (orphan_hash, _) = store.insert_bytes(b"orphan bytes").expect("insert orphan");
        let mut live: HashSet<String> = HashSet::new();
        live.insert(live_hash.clone());
        let mut lines = Vec::new();
        let freed =
            sweep_one_pool(&store, &live, false, "test pool", None, &mut lines).expect("sweep");
        assert!(freed > 0);
        // Live chunk survives; orphan is gone.
        let remaining: Vec<String> = store
            .iter_chunks()
            .expect("iter")
            .into_iter()
            .map(|(h, _)| h)
            .collect();
        assert!(remaining.contains(&live_hash));
        assert!(!remaining.contains(&orphan_hash));
    }

    #[test]
    fn sweep_one_pool_dry_run_keeps_everything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ChunkStore::new(dir.path(), "s3b").expect("chunk store");
        store.insert_bytes(b"orphan").expect("insert");
        let live: HashSet<String> = HashSet::new();
        let mut lines = Vec::new();
        // dry_run reports 0 freed and leaves the chunk in place.
        let freed =
            sweep_one_pool(&store, &live, true, "dry pool", None, &mut lines).expect("dry sweep");
        assert_eq!(freed, 0);
        assert_eq!(store.iter_chunks().expect("iter").len(), 1);
    }

    #[test]
    fn run_local_gc_removes_orphan_chunks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ChunkStore::new(dir.path(), "s3b").expect("chunk store");
        store.insert_bytes(b"an orphan chunk").expect("insert");
        // Empty live set -> the one chunk is an orphan.
        let live: LiveSet = HashMap::new();
        let (lines, freed) = run_local_gc(dir.path(), "s3b", &live, false).expect("run local gc");
        assert!(freed > 0);
        assert!(lines.iter().any(|l| l.contains("deleted local chunk")));
        assert_eq!(store.iter_chunks().expect("iter").len(), 0);
    }
}
