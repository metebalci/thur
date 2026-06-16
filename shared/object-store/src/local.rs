// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Local backend for testing and development
//!
//! This backend stores chunks and manifests in the local filesystem without any storage operations.
//! It implements the ObjectStoreBackend trait but all operations are local - no network calls.
//!
//! This is useful for:
//! - Development and testing without storage dependencies
//! - Air-gapped environments
//! - Scenarios where storage storage is not needed

use crate::Result;
use crate::error::ObjectStoreError;
use crate::object_store_backend::ObjectStoreBackend;
use crate::object_store_config::FailureKind;
use async_trait::async_trait;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, info, warn};

/// Monotonic suffix source for staging temp files. Combined with the
/// process id it makes every staged write target a unique path, so two
/// concurrent writers of the same content-addressed key never clobber
/// each other's in-flight temp file.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Build a unique sibling temp path for an atomic write into `final_path`.
/// Kept in the same directory so the follow-up `rename(2)` stays on one
/// filesystem (rename across mounts is not atomic).
fn temp_path_for(final_path: &Path) -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let mut name = final_path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(format!(".tmp.{pid}.{n}"));
    final_path.with_file_name(name)
}

/// Best-effort fsync of a file's parent directory so a preceding
/// `rename(2)` is durable across power loss. A failure here can't tear
/// the object (the rename already landed), so we swallow it.
fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

/// Atomically materialize `data` at `final_path`: write to a sibling temp
/// file, fsync it, `rename(2)` into place, then fsync the parent dir. A
/// crash or power loss can leave a stray `.tmp.*` file but never a torn
/// object at the final key — the property dedup / recovery probes rely on
/// (a present key must be a complete object).
fn atomic_write_bytes(final_path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = temp_path_for(final_path);
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, final_path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    sync_parent_dir(final_path);
    Ok(())
}

/// Atomic counterpart of `fs::copy`: copy `src` to a sibling temp of
/// `final_path`, fsync, `rename(2)` into place, fsync the parent dir.
/// Returns the copied byte count.
fn atomic_copy(src: &Path, final_path: &Path) -> std::io::Result<u64> {
    let tmp = temp_path_for(final_path);
    if let Err(e) = fs::copy(src, &tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    {
        let f = fs::File::open(&tmp)?;
        f.sync_all()?;
    }
    let size = fs::metadata(&tmp)?.len();
    if let Err(e) = fs::rename(&tmp, final_path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    sync_parent_dir(final_path);
    Ok(size)
}

/// Env var that drives test-only failure injection on the LocalBackend.
///
/// Grammar: comma-separated `kind@pattern` entries. Pattern syntax is a
/// dumb glob: bare `*` matches all, `prefix*` is prefix, `*suffix` is
/// suffix, `*middle*` is contains, anything else is an exact match.
/// Kinds map 1-1 to `FailureKind` (case-insensitive): `auth`, `authz`,
/// `notfound`, `regionmismatch`, `network`, `timeout`, `other`.
///
/// Examples:
///
/// ```text
/// THUR_STORAGE_INJECT_FAIL="auth@chunks/*"
/// THUR_STORAGE_INJECT_FAIL="timeout@chunks/*,authz@manifests/*"
/// THUR_STORAGE_INJECT_FAIL="notfound@*"
/// ```
///
/// Off-by-default; intended for shell tests that drive
/// `vtl/scripts/test-backup-storage-failures.sh` and
/// `vsa/scripts/test-fs-storage-failures.sh`.
pub const INJECT_ENV_VAR: &str = "THUR_STORAGE_INJECT_FAIL";

/// Retry budget per op when injection is wired through `retry_async`.
/// Small on purpose: a smoke test that exhausts the budget on a
/// `timeout@*` rule should complete in under 10 seconds (exponential
/// jittered backoff starting at 1s). Real storage backends use 3–5.
const MAX_LOCAL_INJECT_RETRIES: u32 = 2;

/// Local filesystem backend (no storage operations)
///
/// This backend stores all data locally in a directory structure that mimics
/// the storage storage layout (chunks/, manifests/). All operations are synchronous
/// file I/O wrapped in async.
#[derive(Debug, Clone)]
pub struct LocalBackend {
    /// Root directory for local storage
    root_dir: PathBuf,
    /// Per-op failure-injection plan, read once from `THUR_STORAGE_INJECT_FAIL`
    /// at construction. `None` in any non-test config.
    inject: Option<Arc<InjectionPlan>>,
}

impl LocalBackend {
    /// Create a new LocalBackend
    ///
    /// # Arguments
    /// * `root_dir` - Root directory for storing chunks and manifests
    ///
    /// # Example
    /// ```no_run
    /// use shared_object_store::LocalBackend;
    ///
    /// # async fn example() {
    /// let backend = LocalBackend::new("./.thurvtl/local-backend").await.unwrap();
    /// # }
    /// ```
    pub async fn new<P: AsRef<Path>>(root_dir: P) -> Result<Self> {
        let root_dir = root_dir.as_ref().to_path_buf();

        // Create root directory if it doesn't exist
        if !root_dir.exists() {
            fs::create_dir_all(&root_dir)?;
        }

        let inject = InjectionPlan::from_env(INJECT_ENV_VAR).map(Arc::new);
        if let Some(plan) = inject.as_ref() {
            warn!(
                "LocalBackend: failure-injection active ({} rules from {}={})",
                plan.rules.len(),
                INJECT_ENV_VAR,
                std::env::var(INJECT_ENV_VAR).unwrap_or_default()
            );
        }

        info!("Local backend initialized at: {}", root_dir.display());

        Ok(Self { root_dir, inject })
    }

    /// Get full path for a key
    fn get_path(&self, key: &str) -> PathBuf {
        self.root_dir.join(key)
    }

    /// If the configured injection plan matches `key`, return a classified
    /// synthetic `ObjectStoreError`. Otherwise `Ok(())` and the real op runs.
    fn check_injection(&self, op: &'static str, key: &str) -> Result<()> {
        let Some(plan) = self.inject.as_ref() else {
            return Ok(());
        };
        let Some(kind) = plan.matches(key) else {
            return Ok(());
        };
        Err(synthetic_error(op, key, kind))
    }
}

/// Single `kind@pattern` rule parsed from `THUR_STORAGE_INJECT_FAIL`.
#[derive(Debug)]
struct InjectionRule {
    kind: FailureKind,
    pattern: KeyPattern,
}

/// Parsed glob pattern. Kept dumb on purpose: a real glob crate is
/// overkill for `prefix*` / `*suffix` / `*middle*` / exact.
#[derive(Debug, Clone)]
enum KeyPattern {
    Any,
    Prefix(String),
    Suffix(String),
    Contains(String),
    Exact(String),
}

impl KeyPattern {
    fn parse(raw: &str) -> Self {
        let raw = raw.trim();
        if raw == "*" || raw.is_empty() {
            return KeyPattern::Any;
        }
        let starts = raw.starts_with('*');
        let ends = raw.ends_with('*');
        match (starts, ends) {
            (true, true) => KeyPattern::Contains(raw.trim_matches('*').to_string()),
            (true, false) => KeyPattern::Suffix(raw.trim_start_matches('*').to_string()),
            (false, true) => KeyPattern::Prefix(raw.trim_end_matches('*').to_string()),
            (false, false) => KeyPattern::Exact(raw.to_string()),
        }
    }

    fn matches(&self, key: &str) -> bool {
        match self {
            KeyPattern::Any => true,
            KeyPattern::Prefix(p) => key.starts_with(p),
            KeyPattern::Suffix(s) => key.ends_with(s),
            KeyPattern::Contains(c) => key.contains(c),
            KeyPattern::Exact(e) => key == e,
        }
    }
}

/// Parsed plan loaded once at construction.
#[derive(Debug)]
pub(crate) struct InjectionPlan {
    rules: Vec<InjectionRule>,
}

impl InjectionPlan {
    /// Read the named env var; return `None` if absent or parses to zero
    /// valid rules. A partially-valid spec produces a plan with the
    /// rules that did parse — bad entries log a warn and are skipped so
    /// a typo doesn't silently disable an entire test.
    pub(crate) fn from_env(var: &str) -> Option<Self> {
        let raw = std::env::var(var).ok()?;
        Self::parse(&raw)
    }

    fn parse(spec: &str) -> Option<Self> {
        let mut rules = Vec::new();
        for entry in spec.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let Some((kind_raw, pattern_raw)) = entry.split_once('@') else {
                warn!(
                    "LocalBackend injection: skipping malformed entry '{entry}' (expected kind@pattern)"
                );
                continue;
            };
            let kind = match kind_raw.trim().to_ascii_lowercase().as_str() {
                "auth" => FailureKind::Auth,
                "authz" | "permission" => FailureKind::Authz,
                "notfound" | "not_found" => FailureKind::NotFound,
                "regionmismatch" | "region" => FailureKind::RegionMismatch,
                "network" => FailureKind::Network,
                "timeout" => FailureKind::Timeout,
                "other" => FailureKind::Other,
                other => {
                    warn!("LocalBackend injection: unknown kind '{other}' in entry '{entry}'");
                    continue;
                }
            };
            rules.push(InjectionRule {
                kind,
                pattern: KeyPattern::parse(pattern_raw),
            });
        }
        if rules.is_empty() {
            return None;
        }
        Some(Self { rules })
    }

    fn matches(&self, key: &str) -> Option<FailureKind> {
        self.rules
            .iter()
            .find(|r| r.pattern.matches(key))
            .map(|r| r.kind)
    }
}

/// Build the typed `ObjectStoreError` carrier for an injected `kind`, so
/// the fault-injection backend exercises the exact same structured
/// classify path the real backends now mint — `classify` maps it straight
/// back to `kind` (see [`ObjectStoreError::classified`]).
fn synthetic_error(op: &'static str, key: &str, kind: FailureKind) -> ObjectStoreError {
    ObjectStoreError::classified(
        kind,
        format!("synthetic LocalBackend injection ({op} on key={key})"),
    )
}

/// Map a filesystem `io::Error` to the typed `ObjectStoreError` so the
/// retry layer fails fast on permanent faults instead of burning 3-9 s
/// of jittered backoff on them. A missing object (the corruption /
/// eviction-race a chunk/manifest read must handle) is `NotFound` and a
/// permission error is `Authz` — both classify as permanent and can't
/// heal between attempts; every other IO error stays retryable `Io`.
/// Only Local lacked this mapping, defeating the crate's own
/// fail-fast-on-permanent-errors contract for one backend (issue #266).
fn map_local_io(op: &str, key: &str, e: std::io::Error) -> ObjectStoreError {
    match e.kind() {
        std::io::ErrorKind::NotFound => {
            ObjectStoreError::NotFound(format!("{op}: object {key} not found ({e})"))
        }
        std::io::ErrorKind::PermissionDenied => {
            ObjectStoreError::Authz(format!("{op}: permission denied for {key} ({e})"))
        }
        _ => ObjectStoreError::Io(e),
    }
}

#[async_trait]
impl ObjectStoreBackend for LocalBackend {
    async fn upload_chunk(
        &self,
        key: &str,
        // Owned to satisfy the trait (issue #236). The retry closure below
        // borrows it per attempt, so the local path keeps an inner copy —
        // local is the dev / air-gapped surface, not the cloud ingest path
        // the owned-`data` change targets.
        data: Vec<u8>,
    ) -> Result<(
        u64,
        Option<u64>,
        Option<crate::compression::CompressionAlgo>,
    )> {
        // Wrap in retry_async so injection-driven synthetic errors
        // flow through the same classify-and-retry surface real storage
        // backends use — exposing `failed (attempt N/M):` and
        // `failed with permanent error (...)` log lines that the
        // failure-path shell tests grep for. With no injection the
        // inner closure succeeds on the first try; retry_async
        // returns immediately.
        crate::object_store_helpers::retry_async(
            "upload_chunk",
            MAX_LOCAL_INJECT_RETRIES,
            || async {
                self.check_injection("upload_chunk", key)?;
                let path = self.get_path(key);

                // Create parent directories if needed.
                // The `create_dir_all` is small so it stays sync.
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }

                // Up to 128 MiB per chunk on the LocalBackend path. Write
                // to a sibling temp, fsync, and rename(2) into place so a
                // crash mid-write can never leave a torn object at the
                // final key (which dedup / recovery would trust as a
                // complete upload). The write+fsync+rename is real
                // blocking work, so run it on the blocking pool.
                let data_owned = data.to_vec();
                let dst = path.clone();
                tokio::task::spawn_blocking(move || atomic_write_bytes(&dst, &data_owned))
                    .await
                    .map_err(|e| {
                        crate::ObjectStoreError::Other(format!("spawn_blocking join: {e}"))
                    })??;

                let size = data.len() as u64;
                debug!("Local backend uploaded chunk: {} ({} bytes)", key, size);

                // Local backend never compresses (it's a dev / air-gapped
                // surface). Returns no algorithm so the manifest records
                // the chunk as uncompressed.
                Ok((size, None, None))
            },
        )
        .await
    }

    async fn upload_chunk_zerocopy(&self, key: &str, file_path: &Path) -> Result<u64> {
        crate::object_store_helpers::retry_async(
            "upload_chunk_zerocopy",
            MAX_LOCAL_INJECT_RETRIES,
            || async {
                self.check_injection("upload_chunk_zerocopy", key)?;
                let dest_path = self.get_path(key);

                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent)?;
                }

                // Up to 128 MiB on the chunk path. (`fs::copy` is a real
                // pread/pwrite loop, not a clone(2) reflink — the TODO
                // above tracks that.) Copy to a sibling temp, fsync, and
                // rename(2) into place so a crash mid-copy never leaves a
                // torn object at the final key. Run on the blocking pool.
                let src = file_path.to_path_buf();
                let dst = dest_path.clone();
                let size = tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
                    atomic_copy(&src, &dst)
                })
                .await
                .map_err(|e| {
                    crate::ObjectStoreError::Other(format!("spawn_blocking join: {e}"))
                })??;

                debug!(
                    "Local backend uploaded chunk (zerocopy): {} ({} bytes)",
                    key, size
                );

                Ok(size)
            },
        )
        .await
    }

    async fn download_chunk(&self, key: &str) -> Result<Vec<u8>> {
        crate::object_store_helpers::retry_async(
            "download_chunk",
            MAX_LOCAL_INJECT_RETRIES,
            || async {
                self.check_injection("download_chunk", key)?;
                let path = self.get_path(key);

                // Up to 128 MiB; same reasoning as `upload_chunk`. Map a
                // missing / unreadable file to a permanent error so the
                // retry layer doesn't sleep 3-9 s on an ENOENT that can't
                // heal (issue #266).
                let data = tokio::fs::read(&path)
                    .await
                    .map_err(|e| map_local_io("download_chunk", key, e))?;

                debug!(
                    "Local backend downloaded chunk: {} ({} bytes)",
                    key,
                    data.len()
                );

                Ok(data)
            },
        )
        .await
    }

    async fn download_chunks_parallel(&self, keys: &[String]) -> Result<Vec<Vec<u8>>> {
        // For local backend, "parallel" is just sequential reads
        // Could use rayon or tokio::spawn for true parallelism, but not needed for local files
        let mut results = Vec::with_capacity(keys.len());

        for key in keys {
            let data = self.download_chunk(key).await?;
            results.push(data);
        }

        Ok(results)
    }

    async fn upload_manifest(&self, key: &str, json: &str) -> Result<()> {
        crate::object_store_helpers::retry_async(
            "upload_manifest",
            MAX_LOCAL_INJECT_RETRIES,
            || async {
                self.check_injection("upload_manifest", key)?;
                let path = self.get_path(key);

                // Create parent directories if needed
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }

                // Write manifest JSON atomically (temp + fsync + rename)
                // so a crash mid-write can't leave a truncated manifest at
                // the final key. Manifests are small, so the fsync stays
                // inline rather than on the blocking pool.
                atomic_write_bytes(&path, json.as_bytes())?;

                debug!("Local backend uploaded manifest: {}", key);

                Ok(())
            },
        )
        .await
    }

    async fn download_manifest(&self, key: &str) -> Result<String> {
        crate::object_store_helpers::retry_async(
            "download_manifest",
            MAX_LOCAL_INJECT_RETRIES,
            || async {
                self.check_injection("download_manifest", key)?;
                let path = self.get_path(key);

                let json =
                    fs::read_to_string(&path).map_err(|e| map_local_io("download_manifest", key, e))?;

                debug!("Local backend downloaded manifest: {}", key);

                Ok(json)
            },
        )
        .await
    }

    async fn chunk_exists(&self, key: &str) -> Result<bool> {
        crate::object_store_helpers::retry_async(
            "chunk_exists",
            MAX_LOCAL_INJECT_RETRIES,
            || async {
                self.check_injection("chunk_exists", key)?;
                let path = self.get_path(key);
                Ok(path.exists())
            },
        )
        .await
    }

    async fn list_objects(&self, key_prefix: &str) -> Result<Vec<String>> {
        self.check_injection("list_objects", key_prefix)?;
        let prefix_path = self.get_path(key_prefix);

        // If prefix directory doesn't exist, return empty list
        if !prefix_path.exists() {
            return Ok(Vec::new());
        }

        // The recursive walk can touch 10^5-10^6 files on a multi-TB
        // local pool (the air-gapped production config). Run it on the
        // blocking pool so a GC / verify / DR-discovery / boot-warmup
        // listing can't pin a tokio worker for seconds-to-minutes and
        // stall unrelated iSCSI/NVMe IO scheduled on that worker (#267).
        let root_dir = self.root_dir.clone();
        let results = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            // Walk directory recursively
            fn visit_dirs(dir: &Path, root: &Path, results: &mut Vec<String>) -> Result<()> {
                if dir.is_dir() {
                    let entries = fs::read_dir(dir)?;
                    for entry in entries {
                        let entry = entry?;
                        let path = entry.path();
                        if path.is_dir() {
                            visit_dirs(&path, root, results)?;
                        } else if let Ok(rel_path) = path.strip_prefix(root) {
                            // Forward slashes for Windows compatibility.
                            let key = rel_path.to_string_lossy().replace('\\', "/");
                            results.push(key);
                        }
                    }
                }
                Ok(())
            }
            let mut results = Vec::new();
            visit_dirs(&prefix_path, &root_dir, &mut results)?;
            Ok(results)
        })
        .await
        .map_err(|e| ObjectStoreError::Other(format!("list_objects spawn_blocking join: {e}")))??;

        debug!(
            "Local backend listed {} objects with prefix: {}",
            results.len(),
            key_prefix
        );

        Ok(results)
    }

    async fn delete_object(&self, key: &str) -> Result<()> {
        self.check_injection("delete_object", key)?;
        let path = self.get_path(key);

        if path.exists() {
            fs::remove_file(&path)?;

            debug!("Local backend deleted object: {}", key);
        }

        Ok(())
    }

    fn backend_type(&self) -> &'static str {
        "local"
    }

    async fn lock_state(&self) -> Result<crate::object_store_backend::LockState> {
        // Filesystem has no immutability concept; always off.
        Ok(crate::object_store_backend::LockState::Off)
    }

    fn supports_legal_hold(&self) -> bool {
        false
    }

    async fn set_object_legal_hold(&self, _key: &str, _held: bool) -> Result<()> {
        Err(crate::ObjectStoreError::NotSupported(
            "legal hold is not supported on the local backend (no enforcement primitive)"
                .to_string(),
        ))
    }

    async fn get_object_legal_hold(&self, _key: &str) -> Result<bool> {
        Err(crate::ObjectStoreError::NotSupported(
            "legal hold is not supported on the local backend".to_string(),
        ))
    }

    fn clone_box(&self) -> Box<dyn ObjectStoreBackend> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_local_backend_basic_operations() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path()).await.unwrap();

        // Upload chunk
        let data = b"Hello, Thur VTL!";
        let (size, compressed, applied_algo) = backend
            .upload_chunk("chunks/TEST001/obj-000001.dat", data.to_vec())
            .await
            .unwrap();
        assert_eq!(size, data.len() as u64);
        assert_eq!(compressed, None);
        assert_eq!(applied_algo, None);

        // Check exists
        assert!(
            backend
                .chunk_exists("chunks/TEST001/obj-000001.dat")
                .await
                .unwrap()
        );
        assert!(
            !backend
                .chunk_exists("chunks/TEST001/obj-999999.dat")
                .await
                .unwrap()
        );

        // Download chunk
        let downloaded = backend
            .download_chunk("chunks/TEST001/obj-000001.dat")
            .await
            .unwrap();
        assert_eq!(downloaded, data);

        // Upload manifest
        let manifest_json = r#"{"version": 1, "blocks": []}"#;
        backend
            .upload_manifest("manifests/TEST001/manifest-latest.json", manifest_json)
            .await
            .unwrap();

        // Download manifest
        let downloaded_manifest = backend
            .download_manifest("manifests/TEST001/manifest-latest.json")
            .await
            .unwrap();
        assert_eq!(downloaded_manifest, manifest_json);

        // List objects
        let objects = backend.list_objects("chunks/TEST001/").await.unwrap();
        assert_eq!(objects.len(), 1);
        assert!(objects[0].contains("obj-000001.dat"));

        // Delete object
        backend
            .delete_object("chunks/TEST001/obj-000001.dat")
            .await
            .unwrap();
        assert!(
            !backend
                .chunk_exists("chunks/TEST001/obj-000001.dat")
                .await
                .unwrap()
        );
    }

    use crate::object_store_config::classify;

    #[test]
    fn key_pattern_prefix_suffix_contains_exact() {
        assert!(KeyPattern::parse("chunks/*").matches("chunks/abc"));
        assert!(!KeyPattern::parse("chunks/*").matches("manifests/foo"));
        assert!(
            KeyPattern::parse("*manifest-latest.json").matches("manifests/T1/manifest-latest.json")
        );
        assert!(
            !KeyPattern::parse("*manifest-latest.json").matches("manifests/T1/manifest-0001.json")
        );
        assert!(KeyPattern::parse("*index*").matches("indexes/T1/blocks-p0.idx"));
        assert!(KeyPattern::parse("exact/key").matches("exact/key"));
        assert!(!KeyPattern::parse("exact/key").matches("exact/key/extra"));
        assert!(KeyPattern::parse("*").matches("anything"));
        assert!(KeyPattern::parse("").matches("anything"));
    }

    #[test]
    fn injection_plan_parses_each_kind() {
        let plan = InjectionPlan::parse(
            "auth@chunks/*,authz@indexes/*,notfound@deleted/*,regionmismatch@regional/*,network@unreach/*,timeout@slow/*,other@misc/*",
        )
        .expect("non-empty plan");
        assert_eq!(plan.matches("chunks/abc"), Some(FailureKind::Auth));
        assert_eq!(plan.matches("indexes/foo"), Some(FailureKind::Authz));
        assert_eq!(plan.matches("deleted/zz"), Some(FailureKind::NotFound));
        assert_eq!(
            plan.matches("regional/x"),
            Some(FailureKind::RegionMismatch)
        );
        assert_eq!(plan.matches("unreach/x"), Some(FailureKind::Network));
        assert_eq!(plan.matches("slow/x"), Some(FailureKind::Timeout));
        assert_eq!(plan.matches("misc/x"), Some(FailureKind::Other));
        assert_eq!(plan.matches("untouched/x"), None);
    }

    #[test]
    fn injection_plan_rejects_garbage_but_keeps_valid_neighbors() {
        // Bad kind + missing @ should be dropped silently; valid rule survives.
        let plan = InjectionPlan::parse("garbage@chunks/*,malformed,auth@manifests/*")
            .expect("at least one valid rule");
        assert_eq!(plan.matches("manifests/T1/x"), Some(FailureKind::Auth));
        assert_eq!(plan.matches("chunks/abc"), None);
    }

    #[test]
    fn synthetic_errors_roundtrip_through_classify() {
        for kind in [
            FailureKind::Auth,
            FailureKind::Authz,
            FailureKind::NotFound,
            FailureKind::RegionMismatch,
            FailureKind::Network,
            FailureKind::Timeout,
            FailureKind::Other,
        ] {
            let err = synthetic_error("upload_chunk", "chunks/abc", kind);
            assert_eq!(classify(&err), kind, "round-trip failed for {kind:?}");
        }
    }

    #[tokio::test]
    async fn injection_short_circuits_upload() {
        // Skip the env-var path (the crate denies unsafe_code, so
        // `std::env::set_var` is off-limits in tests). Build the
        // backend by hand with an explicit plan instead — the runtime
        // env-var path is just a thin wrapper that uses the same plan
        // type, covered by `injection_plan_parses_each_kind` above.
        let temp_dir = TempDir::new().unwrap();
        let plan = InjectionPlan::parse("auth@injected/*").expect("valid plan");
        let backend = LocalBackend {
            root_dir: temp_dir.path().to_path_buf(),
            inject: Some(Arc::new(plan)),
        };

        let err = backend
            .upload_chunk("injected/k", b"x".to_vec())
            .await
            .expect_err("expected injected auth failure");
        assert_eq!(classify(&err), FailureKind::Auth);

        // Non-matching keys flow through normally.
        backend
            .upload_chunk("normal/k", b"y".to_vec())
            .await
            .expect("non-matching key must not be injected");
    }

    #[tokio::test]
    async fn zerocopy_upload_copies_file_into_backend() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path()).await.unwrap();

        // Stage a source file outside the backend root.
        let src_dir = TempDir::new().unwrap();
        let src_path = src_dir.path().join("chunk.bin");
        let payload = b"zero-copy payload bytes";
        tokio::fs::write(&src_path, payload).await.unwrap();

        let size = backend
            .upload_chunk_zerocopy("chunks/ZC/obj-1.dat", &src_path)
            .await
            .unwrap();
        assert_eq!(size, payload.len() as u64);

        let got = backend.download_chunk("chunks/ZC/obj-1.dat").await.unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn zerocopy_upload_missing_source_errors() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path()).await.unwrap();
        let err = backend
            .upload_chunk_zerocopy("chunks/ZC/missing.dat", Path::new("/nonexistent/src.bin"))
            .await
            .expect_err("missing source must error");
        // The spawn_blocking copy failure surfaces as an IO error.
        assert!(matches!(err, ObjectStoreError::Io(_)));
    }

    #[tokio::test]
    async fn list_objects_on_absent_prefix_is_empty() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path()).await.unwrap();
        let listed = backend.list_objects("never/created/").await.unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn list_objects_walks_nested_directories() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path()).await.unwrap();
        backend
            .upload_chunk("chunks/T/a/obj-1.dat", b"1".to_vec())
            .await
            .unwrap();
        backend
            .upload_chunk("chunks/T/b/obj-2.dat", b"2".to_vec())
            .await
            .unwrap();
        let mut listed = backend.list_objects("chunks/T/").await.unwrap();
        listed.sort();
        assert_eq!(
            listed,
            vec![
                "chunks/T/a/obj-1.dat".to_string(),
                "chunks/T/b/obj-2.dat".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn delete_absent_object_is_noop() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path()).await.unwrap();
        // Deleting a key that was never written must not error.
        backend.delete_object("chunks/never.dat").await.unwrap();
    }

    #[tokio::test]
    async fn download_missing_chunk_errors() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path()).await.unwrap();
        let err = backend
            .download_chunk("chunks/absent.dat")
            .await
            .expect_err("missing chunk must error");
        // Issue #266: a missing chunk is a permanent NotFound (fail fast),
        // not a retryable Io that burns 3-9 s of backoff.
        assert!(
            matches!(err, ObjectStoreError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
        assert_eq!(
            crate::object_store_config::classify(&err),
            crate::object_store_config::FailureKind::NotFound,
        );
    }

    #[tokio::test]
    async fn lock_state_is_always_off() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path()).await.unwrap();
        assert_eq!(
            backend.lock_state().await.unwrap(),
            crate::object_store_backend::LockState::Off
        );
        assert_eq!(backend.backend_type(), "local");
    }

    #[tokio::test]
    async fn legal_hold_is_not_supported() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path()).await.unwrap();
        assert!(!backend.supports_legal_hold());
        let set_err = backend
            .set_object_legal_hold("chunks/x.dat", true)
            .await
            .expect_err("set legal hold must be unsupported");
        assert!(matches!(set_err, ObjectStoreError::NotSupported(_)));
        let get_err = backend
            .get_object_legal_hold("chunks/x.dat")
            .await
            .expect_err("get legal hold must be unsupported");
        assert!(matches!(get_err, ObjectStoreError::NotSupported(_)));
    }

    #[tokio::test]
    async fn new_creates_root_directory_when_absent() {
        let temp_dir = TempDir::new().unwrap();
        let nested = temp_dir.path().join("a/b/c");
        assert!(!nested.exists());
        let backend = LocalBackend::new(&nested).await.unwrap();
        assert!(nested.exists());
        assert_eq!(backend.backend_type(), "local");
    }

    #[test]
    fn injection_plan_from_env_absent_var_is_none() {
        // A var name guaranteed not to be set yields no plan.
        assert!(InjectionPlan::from_env("THUR_STORAGE_INJECT_FAIL_UNSET_XYZ").is_none());
    }

    #[test]
    fn injection_plan_parse_empty_spec_is_none() {
        assert!(InjectionPlan::parse("").is_none());
        assert!(InjectionPlan::parse("   ,  , ").is_none());
    }

    #[test]
    fn clone_box_yields_local_backend() {
        // Build directly so the test stays sync.
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend {
            root_dir: temp_dir.path().to_path_buf(),
            inject: None,
        };
        let cloned = backend.clone_box();
        assert_eq!(cloned.backend_type(), "local");
    }

    #[tokio::test]
    async fn injection_retryable_kind_exhausts_budget() {
        // A timeout rule is retryable; the op should exhaust the small
        // local retry budget and then surface the synthetic error.
        let temp_dir = TempDir::new().unwrap();
        let plan = InjectionPlan::parse("timeout@slow/*").expect("valid plan");
        let backend = LocalBackend {
            root_dir: temp_dir.path().to_path_buf(),
            inject: Some(Arc::new(plan)),
        };
        let err = backend
            .download_chunk("slow/k")
            .await
            .expect_err("timeout injection must fail after retries");
        assert_eq!(classify(&err), FailureKind::Timeout);
    }

    #[tokio::test]
    async fn injection_blocks_list_and_delete() {
        let temp_dir = TempDir::new().unwrap();
        let plan = InjectionPlan::parse("authz@*").expect("valid plan");
        let backend = LocalBackend {
            root_dir: temp_dir.path().to_path_buf(),
            inject: Some(Arc::new(plan)),
        };
        let list_err = backend
            .list_objects("anything/")
            .await
            .expect_err("list must be injected");
        assert_eq!(classify(&list_err), FailureKind::Authz);
        let del_err = backend
            .delete_object("anything/k")
            .await
            .expect_err("delete must be injected");
        assert_eq!(classify(&del_err), FailureKind::Authz);
    }

    #[tokio::test]
    async fn manifest_download_missing_errors() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path()).await.unwrap();
        let err = backend
            .download_manifest("manifests/absent.json")
            .await
            .expect_err("missing manifest must error");
        // Issue #266: a missing manifest is a permanent NotFound.
        assert!(
            matches!(err, ObjectStoreError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn upload_leaves_no_temp_files_and_content_intact() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path()).await.unwrap();
        let data = vec![0xABu8; 4096];
        backend
            .upload_chunk("chunks/AT/obj-1.dat", data.to_vec())
            .await
            .unwrap();
        let manifest = r#"{"v":1}"#;
        backend
            .upload_manifest("manifests/AT/m.json", manifest)
            .await
            .unwrap();

        // Final keys hold the exact bytes.
        assert_eq!(
            backend.download_chunk("chunks/AT/obj-1.dat").await.unwrap(),
            data
        );
        assert_eq!(
            backend
                .download_manifest("manifests/AT/m.json")
                .await
                .unwrap(),
            manifest
        );

        // No staging temp files survived the atomic rename.
        let leftovers = leftover_temp_files(temp_dir.path());
        assert!(
            leftovers.is_empty(),
            "stray temp files after upload: {leftovers:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_uploads_of_same_key_leave_no_temp_files() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path()).await.unwrap();
        let data = vec![0x5Au8; 8192];
        // Two writers racing on the identical content-addressed key:
        // unique temp suffixes mean neither tears the other's temp file,
        // and the final object is complete regardless of who renamed last.
        let a = {
            let b = backend.clone();
            let d = data.clone();
            tokio::spawn(async move { b.upload_chunk("chunks/RACE/obj.dat", d.to_vec()).await })
        };
        let c = {
            let b = backend.clone();
            let d = data.clone();
            tokio::spawn(async move { b.upload_chunk("chunks/RACE/obj.dat", d.to_vec()).await })
        };
        a.await.unwrap().unwrap();
        c.await.unwrap().unwrap();

        assert_eq!(
            backend.download_chunk("chunks/RACE/obj.dat").await.unwrap(),
            data
        );
        let leftovers = leftover_temp_files(temp_dir.path());
        assert!(
            leftovers.is_empty(),
            "stray temp files after concurrent upload: {leftovers:?}"
        );
    }

    #[tokio::test]
    async fn zerocopy_upload_leaves_no_temp_files() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path()).await.unwrap();
        let src_dir = TempDir::new().unwrap();
        let src_path = src_dir.path().join("chunk.bin");
        let payload = vec![0x11u8; 2048];
        tokio::fs::write(&src_path, &payload).await.unwrap();
        backend
            .upload_chunk_zerocopy("chunks/ZCAT/obj.dat", &src_path)
            .await
            .unwrap();
        assert_eq!(
            backend.download_chunk("chunks/ZCAT/obj.dat").await.unwrap(),
            payload
        );
        let leftovers = leftover_temp_files(temp_dir.path());
        assert!(
            leftovers.is_empty(),
            "stray temp files after zerocopy upload: {leftovers:?}"
        );
    }

    /// Recursively collect any `.tmp.*` staging files under `root`.
    fn leftover_temp_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        walk(&path, out);
                    } else if path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.contains(".tmp."))
                        .unwrap_or(false)
                    {
                        out.push(path);
                    }
                }
            }
        }
        walk(root, &mut out);
        out
    }

    #[tokio::test]
    async fn test_local_backend_parallel_download() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path()).await.unwrap();

        // Upload multiple chunks
        backend
            .upload_chunk("chunks/TEST002/obj-000001.dat", b"chunk1".to_vec())
            .await
            .unwrap();
        backend
            .upload_chunk("chunks/TEST002/obj-000002.dat", b"chunk2".to_vec())
            .await
            .unwrap();
        backend
            .upload_chunk("chunks/TEST002/obj-000003.dat", b"chunk3".to_vec())
            .await
            .unwrap();

        // Download in parallel
        let keys = vec![
            "chunks/TEST002/obj-000001.dat".to_string(),
            "chunks/TEST002/obj-000002.dat".to_string(),
            "chunks/TEST002/obj-000003.dat".to_string(),
        ];
        let chunks = backend.download_chunks_parallel(&keys).await.unwrap();

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], b"chunk1");
        assert_eq!(chunks[1], b"chunk2");
        assert_eq!(chunks[2], b"chunk3");
    }
}
