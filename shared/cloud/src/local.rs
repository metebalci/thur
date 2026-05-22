// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Local backend for testing and development
//!
//! This backend stores chunks and manifests in the local filesystem without any cloud operations.
//! It implements the CloudBackend trait but all operations are local - no network calls.
//!
//! This is useful for:
//! - Development and testing without cloud dependencies
//! - Air-gapped environments
//! - Scenarios where cloud storage is not needed

use crate::Result;
use crate::cloud_backend::CloudBackend;
use crate::cloud_config::FailureKind;
use crate::error::CloudError;
use async_trait::async_trait;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info, warn};

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
/// THUR_CLOUD_INJECT_FAIL="auth@chunks/*"
/// THUR_CLOUD_INJECT_FAIL="timeout@chunks/*,authz@manifests/*"
/// THUR_CLOUD_INJECT_FAIL="notfound@*"
/// ```
///
/// Off-by-default; intended for shell tests that drive
/// `vtl/scripts/test-backup-cloud-failures.sh` and
/// `vsa/scripts/test-fs-cloud-failures.sh`.
pub const INJECT_ENV_VAR: &str = "THUR_CLOUD_INJECT_FAIL";

/// Retry budget per op when injection is wired through `retry_async`.
/// Small on purpose: a smoke test that exhausts the budget on a
/// `timeout@*` rule should complete in under 10 seconds (exponential
/// jittered backoff starting at 1s). Real cloud backends use 3–5.
const MAX_LOCAL_INJECT_RETRIES: u32 = 2;

/// Local filesystem backend (no cloud operations)
///
/// This backend stores all data locally in a directory structure that mimics
/// the cloud storage layout (chunks/, manifests/). All operations are synchronous
/// file I/O wrapped in async.
#[derive(Debug, Clone)]
pub struct LocalBackend {
    /// Root directory for local storage
    root_dir: PathBuf,
    /// Per-op failure-injection plan, read once from `THUR_CLOUD_INJECT_FAIL`
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
    /// use shared_cloud::LocalBackend;
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
    /// synthetic `CloudError`. Otherwise `Ok(())` and the real op runs.
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

/// Single `kind@pattern` rule parsed from `THUR_CLOUD_INJECT_FAIL`.
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

/// Build a `CloudError::Other(...)` whose message contains a token that
/// `cloud_config::classify` will deterministically map back to `kind`.
/// Keeps the synthetic error indistinguishable from a real one as far
/// as the retry classifier is concerned.
fn synthetic_error(op: &'static str, key: &str, kind: FailureKind) -> CloudError {
    let token = match kind {
        FailureKind::Auth => "InvalidAccessKeyId",
        FailureKind::Authz => "AccessDenied",
        FailureKind::NotFound => "NoSuchBucket",
        FailureKind::RegionMismatch => "PermanentRedirect",
        FailureKind::Network => "dispatch failure (io: connection refused)",
        FailureKind::Timeout => "timed out",
        FailureKind::Other => "other",
    };
    CloudError::Other(format!(
        "{token}: synthetic LocalBackend injection ({} on key={key})",
        op
    ))
}

#[async_trait]
impl CloudBackend for LocalBackend {
    async fn upload_chunk(
        &self,
        key: &str,
        data: &[u8],
    ) -> Result<(
        u64,
        Option<u64>,
        Option<crate::compression::CompressionAlgo>,
    )> {
        // Wrap in retry_async so injection-driven synthetic errors
        // flow through the same classify-and-retry surface real cloud
        // backends use — exposing `failed (attempt N/M):` and
        // `failed with permanent error (...)` log lines that the
        // failure-path shell tests grep for. With no injection the
        // inner closure succeeds on the first try; retry_async
        // returns immediately.
        crate::cloud_helpers::retry_async("upload_chunk", MAX_LOCAL_INJECT_RETRIES, || async {
            self.check_injection("upload_chunk", key)?;
            let path = self.get_path(key);

            // Create parent directories if needed.
            // The `create_dir_all` is small so it stays sync.
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }

            // Up to 128 MiB per chunk on the LocalBackend path. Sync
            // `fs::write` would park a tokio worker for the full kernel
            // flush; `tokio::fs::write` runs the syscall on the blocking
            // pool.
            tokio::fs::write(&path, data).await?;

            let size = data.len() as u64;
            debug!("Local backend uploaded chunk: {} ({} bytes)", key, size);

            // Local backend never compresses (it's a dev / air-gapped
            // surface). Returns no algorithm so the manifest records
            // the chunk as uncompressed.
            Ok((size, None, None))
        })
        .await
    }

    async fn upload_chunk_zerocopy(&self, key: &str, file_path: &Path) -> Result<u64> {
        crate::cloud_helpers::retry_async(
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
                // above tracks that.) Run on the blocking pool.
                let src = file_path.to_path_buf();
                let dst = dest_path.clone();
                let size = tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
                    fs::copy(&src, &dst)?;
                    Ok(fs::metadata(&dst)?.len())
                })
                .await
                .map_err(|e| crate::CloudError::Other(format!("spawn_blocking join: {e}")))??;

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
        crate::cloud_helpers::retry_async("download_chunk", MAX_LOCAL_INJECT_RETRIES, || async {
            self.check_injection("download_chunk", key)?;
            let path = self.get_path(key);

            // Up to 128 MiB; same reasoning as `upload_chunk`.
            let data = tokio::fs::read(&path).await?;

            debug!(
                "Local backend downloaded chunk: {} ({} bytes)",
                key,
                data.len()
            );

            Ok(data)
        })
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
        crate::cloud_helpers::retry_async("upload_manifest", MAX_LOCAL_INJECT_RETRIES, || async {
            self.check_injection("upload_manifest", key)?;
            let path = self.get_path(key);

            // Create parent directories if needed
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }

            // Write manifest JSON
            fs::write(&path, json)?;

            debug!("Local backend uploaded manifest: {}", key);

            Ok(())
        })
        .await
    }

    async fn download_manifest(&self, key: &str) -> Result<String> {
        crate::cloud_helpers::retry_async("download_manifest", MAX_LOCAL_INJECT_RETRIES, || async {
            self.check_injection("download_manifest", key)?;
            let path = self.get_path(key);

            let json = fs::read_to_string(&path)?;

            debug!("Local backend downloaded manifest: {}", key);

            Ok(json)
        })
        .await
    }

    async fn chunk_exists(&self, key: &str) -> Result<bool> {
        crate::cloud_helpers::retry_async("chunk_exists", MAX_LOCAL_INJECT_RETRIES, || async {
            self.check_injection("chunk_exists", key)?;
            let path = self.get_path(key);
            Ok(path.exists())
        })
        .await
    }

    async fn list_objects(&self, key_prefix: &str) -> Result<Vec<String>> {
        self.check_injection("list_objects", key_prefix)?;
        let prefix_path = self.get_path(key_prefix);

        // If prefix directory doesn't exist, return empty list
        if !prefix_path.exists() {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();

        // Walk directory recursively
        fn visit_dirs(dir: &Path, root: &Path, results: &mut Vec<String>) -> Result<()> {
            if dir.is_dir() {
                let entries = fs::read_dir(dir)?;

                for entry in entries {
                    let entry = entry?;
                    let path = entry.path();

                    if path.is_dir() {
                        visit_dirs(&path, root, results)?;
                    } else {
                        // Get relative path from root
                        if let Ok(rel_path) = path.strip_prefix(root) {
                            let key = rel_path.to_string_lossy().to_string();
                            // Replace backslashes with forward slashes for Windows compatibility
                            let key = key.replace('\\', "/");
                            results.push(key);
                        }
                    }
                }
            }
            Ok(())
        }

        visit_dirs(&prefix_path, &self.root_dir, &mut results)?;

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

    async fn lock_state(&self) -> Result<crate::cloud_backend::LockState> {
        // Filesystem has no immutability concept; always off.
        Ok(crate::cloud_backend::LockState::Off)
    }

    fn supports_legal_hold(&self) -> bool {
        false
    }

    async fn set_object_legal_hold(&self, _key: &str, _held: bool) -> Result<()> {
        Err(crate::CloudError::NotSupported(
            "legal hold is not supported on the local backend (no enforcement primitive)"
                .to_string(),
        ))
    }

    async fn get_object_legal_hold(&self, _key: &str) -> Result<bool> {
        Err(crate::CloudError::NotSupported(
            "legal hold is not supported on the local backend".to_string(),
        ))
    }

    fn clone_box(&self) -> Box<dyn CloudBackend> {
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
            .upload_chunk("chunks/TEST001/obj-000001.dat", data)
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

    use crate::cloud_config::classify;

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
            .upload_chunk("injected/k", b"x")
            .await
            .expect_err("expected injected auth failure");
        assert_eq!(classify(&err), FailureKind::Auth);

        // Non-matching keys flow through normally.
        backend
            .upload_chunk("normal/k", b"y")
            .await
            .expect("non-matching key must not be injected");
    }

    #[tokio::test]
    async fn test_local_backend_parallel_download() {
        let temp_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(temp_dir.path()).await.unwrap();

        // Upload multiple chunks
        backend
            .upload_chunk("chunks/TEST002/obj-000001.dat", b"chunk1")
            .await
            .unwrap();
        backend
            .upload_chunk("chunks/TEST002/obj-000002.dat", b"chunk2")
            .await
            .unwrap();
        backend
            .upload_chunk("chunks/TEST002/obj-000003.dat", b"chunk3")
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
