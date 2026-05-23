// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! thurvsa admin-socket request handlers.
//!
//! Surface today (post-Step 3d):
//!
//! - `GET  /api/v1/health`              — daemon heartbeat
//! - `GET  /api/v1/volumes`             — runtime LUN map snapshot
//! - `GET  /api/v1/volumes/{name}`      — one volume's manifest
//! - `POST /api/v1/volumes`             — live volume create
//! - `DELETE /api/v1/volumes/{name}`    — live volume destroy
//!
//! Live create / destroy mutate the same `Arc<VolumeRegistry>` the
//! SCSI dispatcher reads from on every command. Concurrency is
//! handled inside the registry (`RwLock`); admin handlers only ever
//! see the high-level `register` / `unregister_by_name` API.
//!
//! Per-create cloud-backend instantiation reuses the cache the
//! daemon's discovery pass populated at boot. New backend names not
//! seen at boot are instantiated on first create and cached for the
//! life of the daemon, so a follow-up create against the same
//! backend reuses the same authenticated client.
//!
//! Audit: every mutation (`volume.create`, `volume.destroy`)
//! produces an audit entry through the shared `AuditChannel` when
//! audit is enabled. Read endpoints don't audit (would balloon the
//! chain on every CLI poll).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, extract::Path as AxumPath, extract::State, http::StatusCode, response::IntoResponse,
};
use core_block::{
    self, PageCache, UploadTask, VolumeManifest, VolumeRuntime, VolumeWriter,
    volume::{VolumeEncryptionAlgorithm, parse_dedup_scope},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use shared_admin_server::{JobRegistry, PeerCred};
use shared_audit::{AuditActor, AuditChannel, AuditResult};
use shared_cloud::{CloudBackend, CloudConfig};
use shared_keystore::{DekSource, KeyStoreBackend, KeystoreYamlConfig, SecretBytes};
use shared_pool::PoolBudget;
use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{info, warn};

use crate::registry::VolumeRegistry;

/// Shared state injected into every admin handler. Cheap to clone —
/// every field is already wrapped in its own `Arc`.
#[derive(Clone)]
pub struct AdminState {
    pub data_dir: PathBuf,
    pub cloud: Arc<CloudConfig>,
    pub registry: Arc<VolumeRegistry>,
    pub backends: Arc<Mutex<BTreeMap<String, Arc<dyn CloudBackend>>>>,
    pub audit: Option<AuditChannel>,
    /// Audit-log directory (`<data_dir>/audit` or the `audit.dir`
    /// override). The `system.audit.*` job handlers read JSONL files
    /// from here; populated even when `audit.enabled=false` so the
    /// verb reports a clear "audit dir not found".
    pub audit_dir: PathBuf,
    /// Long-running admin jobs registry — wired into the shared
    /// `jobs_router`. Job kinds (`system.alerting.test`, future
    /// `system.gc` / `verify`) drop into `admin::job_dispatch`.
    pub jobs: Arc<JobRegistry>,
    /// Parsed `keystore:` block of the YAML conffile — the named map
    /// `volume create` resolves `--keystore NAME` against.
    pub keystore_config: Arc<KeystoreYamlConfig>,
    /// Per-backend cache — instantiated lazily on first use, cloned
    /// by handlers thereafter so KMS / Vault clients are reused for
    /// the daemon's lifetime.
    pub keystore_cache: Arc<RwLock<BTreeMap<String, Arc<dyn KeyStoreBackend>>>>,
    /// Unix epoch seconds the daemon started at. Captured once at
    /// boot — surfaces in the `system monitor` header (uptime = now
    /// - started_at).
    pub started_at_unix: i64,
    /// Per-backend disk-cache budgets. Same `Arc<PoolBudget>` map the
    /// eviction worker holds; cloned in once at boot so the monitor
    /// handler can read used / cap / waiters_now coherently AND so
    /// runtime `volume create` can chain `.with_pool_budget(...)`
    /// onto the new `VolumeWriter` (matching the boot path in
    /// `discovery.rs`).
    pub pool_budgets: HashMap<String, Arc<PoolBudget>>,
    /// `try_reserve` deadline applied to every `VolumeWriter`'s pool
    /// budget. Parsed once at boot from
    /// `cfg.disk_cache.backpressure_max_wait_seconds`; threaded here
    /// so runtime `volume create` builds writers with the same
    /// backpressure semantics as boot.
    pub backpressure_deadline: Duration,
    /// Sender end of the async upload-worker channel. Cloned into
    /// each runtime-created `VolumeWriter` so its
    /// `write_page_unsynced` takes the async dispatch path (vs.
    /// falling back to the inline upload, which blocks the SCSI
    /// write on cloud and skips the `backend_bytes_written`
    /// counter).
    pub upload_tx: mpsc::Sender<UploadTask>,
    /// iSCSI / NVMe-TCP session manager. Cloned from the transport's
    /// `Arc<SessionManager>` so the monitor handler can read
    /// `session_count` without going through the HTTP listener state.
    pub sessions: Arc<shared_iscsi::session::SessionManager>,
}

// `system.monitor` per-tick view. The handler in `shared-admin-monitor`
// calls these accessors once per second to compose the JSON payload.
impl shared_admin_monitor::MonitorState for AdminState {
    fn daemon_name(&self) -> &str {
        "thurvsad"
    }
    fn version(&self) -> &str {
        crate::THURVSA_VERSION_STR
    }
    fn started_at_unix(&self) -> i64 {
        self.started_at_unix
    }
    fn live_stats(&self) -> Arc<shared_telemetry::LiveStats> {
        // The global is always set on the daemon side (see main.rs
        // boot); fallback keeps the `--test` smoke path harmless.
        shared_telemetry::global()
            .map(|t| t.live_stats())
            .unwrap_or_else(|| Arc::new(shared_telemetry::LiveStats::default()))
    }
    fn pool_budgets(
        &self,
    ) -> std::collections::HashMap<String, Arc<shared_pool::PoolBudget>> {
        self.pool_budgets.clone()
    }
    fn snapshot_product(&self) -> shared_admin_monitor::ProductSnapshot {
        shared_admin_monitor::ProductSnapshot::Vsa {
            volumes_online: self.registry.len() as u64,
            sessions_active: self.sessions.session_count() as u64,
        }
    }
}

impl AdminState {
    /// Resolve `--keystore NAME` (or absence) into a live backend
    /// handle. Mirrors `shared_cloud::is_single_backend` inference.
    /// Lazily instantiates the backend the first time it's
    /// referenced and caches the result for subsequent calls.
    pub async fn resolve_keystore_backend(
        &self,
        explicit: Option<&str>,
    ) -> anyhow::Result<(String, Arc<dyn KeyStoreBackend>)> {
        let name = self
            .keystore_config
            .resolve_name(explicit)
            .map_err(|e| anyhow::anyhow!("resolve keystore backend: {e}"))?
            .to_string();
        if let Some(b) = self.keystore_cache.read().await.get(&name) {
            return Ok((name, Arc::clone(b)));
        }
        // Slow path: instantiate, then cache.
        let boxed = self
            .keystore_config
            .create_backend_named(&name, &self.data_dir)
            .await
            .map_err(|e| anyhow::anyhow!("instantiate keystore backend '{name}': {e}"))?;
        let arc: Arc<dyn KeyStoreBackend> = Arc::from(boxed);
        self.keystore_cache
            .write()
            .await
            .insert(name.clone(), Arc::clone(&arc));
        Ok((name, arc))
    }
}

/// `GET /api/v1/health` — admin-side health probe.
///
/// Distinct from the Prometheus `/metrics` endpoint and from any
/// future TCP `/health`. Authentication is the socket's filesystem
/// permissions; the response carries enough context for the CLI to
/// confirm it's talking to the right `data_dir`.
pub async fn health(State(state): State<AdminState>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "daemon": "thurvsad",
        "version": env!("CARGO_PKG_VERSION"),
        "data_dir": state.data_dir,
        "api_version": "v1",
        "volume_count": state.registry.len(),
    }))
}

/// `GET /api/v1/volumes` — list every registered volume in LUN
/// order. Reads the live registry, not the disk — daemon-down list
/// (CLI fallback when the socket isn't reachable) walks
/// `<data_dir>/volumes/` directly.
pub async fn list(State(state): State<AdminState>) -> impl IntoResponse {
    let entries = state.registry.entries();
    let volumes: Vec<VolumeRow> = entries
        .into_iter()
        .map(|(lun, c)| VolumeRow::from_cache(lun, &c))
        .collect();
    Json(json!({ "volumes": volumes }))
}

/// `GET /api/v1/volumes/{name}` — one volume's manifest as JSON.
///
/// Carries the creation-frozen `manifest.json` fields plus the
/// assigned `lun`, the on-disk `path`, and an embedded `runtime`
/// block so CLI consumers don't have to make a second hop for the
/// byte counters and last-modified timestamp. For an attached
/// volume the `runtime` block is a live snapshot of the cache's
/// counters; for a detached one it is the `runtime.json` sidecar.
pub async fn info(
    State(state): State<AdminState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let manifest = VolumeManifest::load(&state.data_dir, &name).map_err(|e| {
        let status = match e {
            core_block::VolumeError::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": e.to_string() })))
    })?;
    let vol_dir = VolumeManifest::dir_for(&state.data_dir, &name);
    // An attached volume's live byte counters lead the on-disk
    // sidecar (which only catches up at flush boundaries / the 60 s
    // timer), so prefer the cache's snapshot; fall back to
    // `runtime.json` when the volume isn't currently open.
    let attached = state
        .registry
        .entries()
        .into_iter()
        .find(|(_, w)| w.manifest().name == name);
    let lun = attached.as_ref().map(|(lun, _)| *lun);
    let runtime = match &attached {
        Some((_, cache)) => cache.runtime_snapshot(),
        None => VolumeRuntime::load(&vol_dir).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?,
    };
    let mut out = serde_json::to_value(&manifest).unwrap_or_else(|_| json!({}));
    if let Some(map) = out.as_object_mut() {
        map.insert("lun".into(), json!(lun));
        map.insert("path".into(), json!(vol_dir.display().to_string()));
        map.insert(
            "runtime".into(),
            serde_json::to_value(&runtime).unwrap_or(json!({})),
        );
    }
    Ok(Json(out))
}

/// Resolve the cloud backend name: explicit request field wins;
/// otherwise auto-pick when exactly one backend is configured; refuse
/// when 2+ backends are configured without an explicit choice.
fn resolve_backend(state: &AdminState, req_backend: &Option<String>) -> Result<String, String> {
    let backend_names = state.cloud.backend_names();
    match (req_backend, backend_names.len()) {
        (Some(name), _) => {
            if !backend_names.iter().any(|n| n == name) {
                return Err(format!(
                    "backend '{}' not configured. Available: {}",
                    name,
                    backend_names.join(", ")
                ));
            }
            Ok(name.clone())
        }
        (None, 1) => Ok(backend_names.into_iter().next().unwrap_or_default()),
        (None, _) => Err(format!(
            "backend is required when multiple cloud backends are configured. Available: {}",
            backend_names.join(", ")
        )),
    }
}

/// `POST /api/v1/volumes` — create a new volume on disk and
/// register it for live SCSI dispatch.
///
/// Body shape mirrors the CLI surface (`thurvsa volume create`).
/// On success the response carries the freshly-assigned LUN and a
/// summary of the manifest. Errors are mapped to HTTP status codes
/// the CLI surfaces verbatim.
pub async fn create(
    State(state): State<AdminState>,
    peer: PeerCred,
    Json(body): Json<CreateVolumeRequest>,
) -> Result<(StatusCode, Json<VolumeRow>), (StatusCode, Json<serde_json::Value>)> {
    let dedup_scope = parse_dedup_scope(&body.dedup).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("invalid dedup scope: {e}") })),
        )
    })?;
    let sync_after = match body.sync_after.as_deref() {
        None => core_block::SyncAfter::default(),
        Some(s) => s.parse::<core_block::SyncAfter>().map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid sync_after: {e}") })),
            )
        })?,
    };

    let backend = resolve_backend(&state, &body.backend)
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))))?;

    // Look up the pool budget for the resolved backend. The boot
    // path makes the same invariant assumption (every backend in
    // cloud_config has a budget in pool_budgets, built side-by-side
    // in main.rs); refuse the create fast if it's somehow missing
    // rather than carrying half-wired state into VolumeWriter.
    let pool_budget = state.pool_budgets.get(&backend).cloned().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": format!(
                    "no pool budget configured for backend '{}' (internal: boot path should have wired one)",
                    backend
                )
            })),
        )
    })?;

    // Pin the LUN before we touch the disk so an unavailable
    // explicit `--lun N` refuses cleanly without leaving a
    // half-created volume directory. Auto-assign picks the smallest
    // gap (registry.next_free_lun); explicit pins are validated
    // against the live registry and rejected with 409 on collision.
    let pinned_lun = match body.lun {
        Some(req) => {
            if state.registry.get(req).is_some() {
                return Err((
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": format!("lun {} already bound to another volume", req)
                    })),
                ));
            }
            req
        }
        None => state.registry.next_free_lun(),
    };

    // Decode an operator-supplied DEK once up front so a malformed
    // `key_hex` refuses with a clean BAD_REQUEST before we touch the
    // disk.
    let supplied_key: Option<[u8; shared_crypto::KEY_LEN]> = match body.key_hex.as_deref() {
        None => None,
        Some(hex_str) => {
            let trimmed = hex_str.trim();
            if trimmed.len() != shared_crypto::KEY_LEN * 2 {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": format!(
                            "key_hex must be {} hex chars (AES-256), got {}",
                            shared_crypto::KEY_LEN * 2,
                            trimmed.len(),
                        )
                    })),
                ));
            }
            let mut bytes = [0u8; shared_crypto::KEY_LEN];
            hex::decode_to_slice(trimmed, &mut bytes).map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("key_hex is not valid hex: {e}") })),
                )
            })?;
            Some(bytes)
        }
    };

    // Encryption is opt-in: `encrypt` is the sole trigger and it
    // requires a keystore. `key_hex` / `keystore` / `dek_source` are
    // modifiers that only make sense under `encrypt` — the CLI's clap
    // `requires` enforces this; mirror it here for the raw admin API.
    if body.encrypt && body.keystore.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "encrypt requires a keystore — set `keystore`" })),
        ));
    }
    if !body.encrypt
        && (supplied_key.is_some() || body.keystore.is_some() || body.dek_source.is_some())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "key_hex / keystore / dek_source require `encrypt: true`"
            })),
        ));
    }
    let encrypt_on = body.encrypt;

    let dek_source = match body.dek_source.as_deref() {
        Some(s) => DekSource::parse(s).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!(
                        "dek_source must be 'daemon' or 'backend', got '{s}'"
                    )
                })),
            )
        })?,
        None => DekSource::Daemon,
    };

    // Resolve the keystore backend BEFORE the manifest is written so
    // an unknown-keystore name refuses cleanly without leaving a
    // half-created volume directory.
    let resolved_keystore = if encrypt_on {
        Some(
            state
                .resolve_keystore_backend(body.keystore.as_deref())
                .await
                .map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": e.to_string() })),
                    )
                })?,
        )
    } else {
        None
    };

    let mut manifest = VolumeManifest::new(
        body.name.clone(),
        body.size_bytes,
        core_block::volume::DEFAULT_SECTOR_BYTES,
        body.page_size_bytes,
        backend.clone(),
        dedup_scope,
        body.worm,
        pinned_lun,
    )
    .map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    if encrypt_on {
        manifest = manifest.with_encryption(VolumeEncryptionAlgorithm::Aes256Gcm);
    }

    let created = manifest.create(&state.data_dir).map_err(|e| {
        let status = match e {
            core_block::VolumeError::AlreadyExists(_) => StatusCode::CONFLICT,
            _ => StatusCode::BAD_REQUEST,
        };
        (status, Json(json!({ "error": e.to_string() })))
    })?;

    // The just-created runtime.json carries `sync_after = Cloud`
    // (VolumeRuntime::new_zero). Rewrite it with the operator's
    // choice before the VolumeWriter opens, so the atomic boots up
    // on the right tier. Failure here rolls back the volume —
    // half-created state would leave the operator without a
    // recovery path.
    if sync_after != core_block::SyncAfter::default() {
        let runtime = VolumeRuntime::new_zero_with_sync_after(sync_after);
        let vol_dir = VolumeManifest::dir_for(&state.data_dir, &created.name);
        if let Err(e) = runtime.persist(&vol_dir) {
            let _ = std::fs::remove_dir_all(&vol_dir);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": format!("write runtime.json with sync_after: {e}")
                })),
            ));
        }
    }

    // Mint or wrap the DEK via the resolved backend. The plaintext
    // goes straight into VolumeWriter; the wrapped blob (if any) is
    // stamped into the manifest below. For `local` the wrap is a
    // side-effect file write — `wrapped_blob` comes back empty.
    let (encryption_plain, encryption_wrap, keystore_name_for_audit, backend_for_open) =
        if let Some((name, backend)) = resolved_keystore {
            let result = match supplied_key {
                Some(bytes) => {
                    let plain = SecretBytes::new(bytes);
                    let wrapped = backend.wrap(&created.uuid, &plain).await;
                    wrapped.map(|w| (plain, w))
                }
                None => backend.generate_and_wrap(&created.uuid, dek_source).await,
            };
            match result {
                Ok((plain, wrapped)) => (
                    Some(plain),
                    Some(wrapped),
                    Some(name),
                    Some(Arc::clone(&backend)),
                ),
                Err(e) => {
                    // Roll back the volume directory — manifest
                    // already on disk but no DEK behind it.
                    let _ = std::fs::remove_dir_all(VolumeManifest::dir_for(
                        &state.data_dir,
                        &created.name,
                    ));
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("keystore wrap: {e}") })),
                    ));
                }
            }
        } else {
            (None, None, None, None)
        };

    // For non-local backends, stamp the wrapped DEK back into the
    // manifest so discovery can re-derive the plaintext at boot.
    // Local keeps `wrapped_dek = None` (the sidecar file is the
    // storage).
    let mut created = created;
    if let (Some(name), Some(backend)) =
        (keystore_name_for_audit.as_ref(), backend_for_open.as_ref())
        && created.encryption.is_some()
    {
        let wrapped_b64 = if backend.manages_local_blob() {
            None
        } else {
            encryption_wrap
                .as_ref()
                .map(|w| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, w))
        };
        created = created.with_keystore(name.clone(), wrapped_b64);
        let vol_dir = VolumeManifest::dir_for(&state.data_dir, &created.name);
        if let Err(e) = created.persist(&vol_dir) {
            let _ = std::fs::remove_dir_all(&vol_dir);
            let _ = backend.forget(&created.uuid).await;
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("persist manifest: {e}") })),
            ));
        }
    }

    // Resolve the cloud backend — reuse the cached client when
    // already instantiated, otherwise instantiate now and cache for
    // future creates.
    let cloud_backend = match get_or_init_backend(&state, &backend).await {
        Ok(b) => b,
        Err(e) => {
            // Roll back the volume directory + any keystore entry so
            // a stuck create doesn't leave half-created state on disk.
            let _ =
                std::fs::remove_dir_all(VolumeManifest::dir_for(&state.data_dir, &created.name));
            if let Some(b) = backend_for_open.as_ref() {
                let _ = b.forget(&created.uuid).await;
            }
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("backend init: {e}") })),
            ));
        }
    };

    let writer = match encryption_plain {
        Some(key) => VolumeWriter::open_with_key(
            &state.data_dir,
            &created.name,
            cloud_backend,
            *key.as_bytes(),
        ),
        None => VolumeWriter::open(&state.data_dir, &created.name, cloud_backend),
    };
    let writer = match writer {
        Ok(w) => {
            // Mirror discovery.rs:269-282 — both builders are
            // required for the volume to use the async upload path
            // (`write_page_unsynced` enqueues to the worker) instead
            // of the inline path (blocking SCSI WRITE on cloud + no
            // `backend_bytes_written` bump). Their absence was a
            // silent runtime-create regression that left every
            // post-boot volume with a flat counter.
            let w = w
                .with_pool_budget(Arc::clone(&pool_budget), state.backpressure_deadline)
                .with_upload_sender(state.upload_tx.clone());
            Arc::new(w)
        }
        Err(e) => {
            let _ =
                std::fs::remove_dir_all(VolumeManifest::dir_for(&state.data_dir, &created.name));
            if let Some(b) = backend_for_open.as_ref() {
                let _ = b.forget(&created.uuid).await;
            }
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("open volume writer: {e}") })),
            ));
        }
    };
    // Resolve the same `max_concurrent` the boot path uses (auto-scale
    // sentinel + explicit override semantics live in shared-cloud's
    // UploadConfig). The source string is intentionally discarded —
    // it's already logged once at boot; per-volume log lines would be
    // noise.
    let (max_concurrent_flushes, _) = state.cloud.upload.resolve_max_concurrent();
    let cache = PageCache::with_budget_and_concurrency(
        writer,
        core_block::DEFAULT_CACHE_BUDGET_BYTES,
        max_concurrent_flushes,
    );
    // Spawn a background flush worker for the new volume. Sibling
    // volumes get theirs at boot in `main.rs`; live-created volumes
    // need it here so writes from the very next host command get the
    // same write-back behavior.
    tokio::spawn(Arc::clone(&cache).run_flush_worker());

    // Use the LUN we pinned up front and persisted into the
    // manifest. A concurrent `volume create` between `pinned_lun`
    // resolution and `register` could in principle race onto the
    // same slot for the auto-assign path; we surface that as a loud
    // warning. The CLI / admin layer serializes creates today, so
    // this branch should be unreachable.
    let lun = created.lun;
    if state.registry.register(lun, Arc::clone(&cache)).is_some() {
        warn!("admin: LUN {} double-bound during volume create", lun);
    }

    info!(
        "admin: created volume '{}' (LUN {}) backend='{}' size={} B uid={} pid={:?}",
        created.name, lun, created.backend, created.size_bytes, peer.uid, peer.pid,
    );

    if let Some(channel) = state.audit.as_ref() {
        // Include encryption metadata (algorithm + key id) on
        // encrypted creates so an auditor can reconcile a key file
        // in the keystore against a volume creation event. Never
        // log the key bytes themselves — those go on disk in the
        // keystore, mode 0600.
        let mut payload = json!({
            "volume": created.name,
            "lun": lun,
            "size_bytes": created.size_bytes,
            "page_size_bytes": created.page_size_bytes,
            "backend": created.backend,
            "dedup_scope": created.dedup_scope.as_str(),
            "worm": created.worm,
            "uuid": hex::encode(created.uuid),
        });
        if let Some(enc) = created.encryption.as_ref() {
            payload["encryption"] = json!({
                "algorithm": enc.algorithm.as_str(),
                "key_id": hex::encode(created.uuid),
                "key_source": if body.key_hex.is_some() {
                    "operator"
                } else {
                    "daemon_generated"
                },
                "keystore_backend": enc.keystore_backend,
                "dek_source": dek_source.as_str(),
                "wrapped_dek_present": enc.wrapped_dek.is_some(),
            });
        }
        channel.try_append(
            "volume.create",
            AuditActor::cli(peer.audit_descriptor()),
            payload,
            AuditResult::Ok,
        );
    }

    Ok((
        StatusCode::CREATED,
        Json(VolumeRow::from_cache(lun, &cache)),
    ))
}

/// `DELETE /api/v1/volumes/{name}` — unregister a live volume and
/// remove its on-disk artifacts (manifest + page index).
///
/// Per-volume chunks under `<data_dir>/chunks/<backend>/<volume>/`
/// (Local dedup scope) or in the shared per-backend pool (Global)
/// are left in place — the future `system gc` sweep will reclaim
/// orphaned chunks. Cloud objects matching the volume namespace are
/// likewise out of scope for this primitive; operators who want to
/// reclaim cloud bytes today can run a backend-specific cleanup
/// against the volume's UUID prefix.
///
/// Today there is no host-quiescence safety check: the dispatcher
/// will simply start returning LU NOT SUPPORTED for the LUN as
/// soon as the registry is updated, and any in-flight commands
/// against the writer drain through the cloned `Arc`. Plan to wire
/// a `?force=true` gate when session-tracking lands.
pub async fn destroy(
    State(state): State<AdminState>,
    peer: PeerCred,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let (lun, cache) = state.registry.unregister_by_name(&name).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("volume '{name}' is not registered") })),
        )
    })?;

    // Flush dirty pages and stop the worker before tearing the
    // volume down — host-acked writes that haven't yet reached cloud
    // would otherwise be silently lost on a destroy. We don't fail
    // the request on flush errors: the chunks we couldn't upload
    // would be orphaned by the destroy anyway, so a warning is
    // truthful.
    if let Err(e) = cache.flush_all().await {
        warn!(
            volume = name.as_str(),
            error = %e,
            "admin: final flush before destroy failed (may have lost host-acked writes)"
        );
    }
    cache.request_shutdown();

    // Drop the cache before removing the volume directory: the
    // `PageIndex` holds an open file handle on `pages.idx` and
    // `remove_dir_all` works fine with open handles on Linux but
    // dropping first keeps the order obvious.
    let backend_name = cache.manifest().backend.clone();
    let uuid = cache.manifest().uuid;
    let uuid_hex = hex::encode(uuid);
    let encryption = cache.manifest().encryption.clone();
    drop(cache);

    // Wipe the at-rest key first — once the volume dir is gone the
    // keystore entry is orphaned and an operator would have to clean
    // it up by hand. Idempotent on missing, so a re-destroy on a
    // partially-removed volume still converges.
    if let Some(enc) = encryption.as_ref() {
        match state
            .resolve_keystore_backend(Some(enc.keystore_backend.as_str()))
            .await
        {
            Ok((_, backend)) => {
                if let Err(e) = backend.forget(&uuid).await {
                    warn!(
                        volume = name.as_str(),
                        error = %e,
                        keystore = enc.keystore_backend.as_str(),
                        "admin: keystore forget failed during volume destroy; \
                         manual cleanup of the {} backend may be needed for uuid {}",
                        enc.keystore_backend, uuid_hex
                    );
                }
            }
            Err(e) => {
                warn!(
                    volume = name.as_str(),
                    error = %e,
                    keystore = enc.keystore_backend.as_str(),
                    "admin: could not resolve keystore backend for destroy; \
                     wrapped DEK in manifest is harmless on its own but \
                     operator may want to verify",
                );
            }
        }
    }

    let vol_dir = VolumeManifest::dir_for(&state.data_dir, &name);
    if let Err(e) = std::fs::remove_dir_all(&vol_dir) {
        warn!(
            "admin: volume '{}' unregistered but on-disk dir remove failed: {}",
            name, e
        );
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": format!("removed from registry but failed to delete {}: {e}", vol_dir.display()),
                "lun": lun,
            })),
        ));
    }

    info!(
        "admin: destroyed volume '{}' (LUN {}) backend='{}' uid={} pid={:?}",
        name, lun, backend_name, peer.uid, peer.pid
    );

    if let Some(channel) = state.audit.as_ref() {
        channel.try_append(
            "volume.destroy",
            AuditActor::cli(peer.audit_descriptor()),
            json!({
                "volume": name,
                "lun": lun,
                "backend": backend_name,
                "uuid": uuid_hex,
            }),
            AuditResult::Ok,
        );
    }

    Ok(Json(json!({
        "volume": name,
        "lun": lun,
        "status": "destroyed",
    })))
}

/// `POST /api/v1/volumes/:name/sync-after` — flip the volume's
/// SCSI SYNCHRONIZE CACHE durability tier at runtime.
///
/// Body shape: `{ "mode": "cloud" | "disk" | "memory" }`. The
/// handler updates the `VolumeWriter`'s atomic + rewrites
/// `runtime.json` so the choice survives a daemon restart. The
/// flip takes effect on the next SYNC; in-flight SYNCs finish
/// under the mode that was active when they started.
///
/// The contract change is **not signalled to the SCSI initiator**
/// — a host fsync-heavy workload silently gains or loses
/// durability on a flip.
pub async fn set_sync_after(
    State(state): State<AdminState>,
    peer: PeerCred,
    AxumPath(name): AxumPath<String>,
    Json(body): Json<SetSyncAfterRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let mode = body.mode.parse::<core_block::SyncAfter>().map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("invalid mode: {e}") })),
        )
    })?;

    let cache = state.registry.get_by_name(&name).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("volume '{name}' is not registered") })),
        )
    })?;

    let previous = cache.writer().sync_after();
    cache.writer().set_sync_after(mode).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("persist sync_after: {e}") })),
        )
    })?;

    if let Some(channel) = state.audit.as_ref() {
        channel.try_append(
            "volume.sync_after.modified",
            AuditActor::cli(peer.audit_descriptor()),
            json!({
                "name": name,
                "previous": previous.as_str(),
                "new": mode.as_str(),
            }),
            AuditResult::Ok,
        );
    }

    info!(
        volume = name.as_str(),
        previous = previous.as_str(),
        new = mode.as_str(),
        "admin: sync_after flipped (silent to the SCSI initiator)"
    );

    Ok(Json(json!({
        "volume": name,
        "previous": previous.as_str(),
        "sync_after": mode.as_str(),
    })))
}

#[derive(Debug, Deserialize)]
pub struct SetSyncAfterRequest {
    pub mode: String,
}

/// Lookup or instantiate the `Arc<dyn CloudBackend>` for `name`.
/// The cache is populated at boot from `discovery.rs`; this helper
/// exists so admin-side creates against a never-before-used backend
/// pay the SDK construction cost exactly once.
async fn get_or_init_backend(
    state: &AdminState,
    name: &str,
) -> anyhow::Result<Arc<dyn CloudBackend>> {
    {
        let cache = state.backends.lock().await;
        if let Some(existing) = cache.get(name) {
            return Ok(Arc::clone(existing));
        }
    }
    let boxed = state
        .cloud
        .create_backend_named(name)
        .await
        .map_err(|e| anyhow::anyhow!("instantiate backend '{}': {}", name, e))?;
    let arc: Arc<dyn CloudBackend> = Arc::from(boxed);
    let mut cache = state.backends.lock().await;
    cache.insert(name.to_string(), Arc::clone(&arc));
    Ok(arc)
}

#[derive(Debug, Deserialize)]
pub struct CreateVolumeRequest {
    pub name: String,
    pub size_bytes: u64,
    #[serde(default = "default_page_size_bytes")]
    pub page_size_bytes: u32,
    /// Cloud backend name from `cloud.backends:` in the YAML
    /// conffile. `None` (operator omitted `--backend`) is resolved
    /// daemon-side: inferred when exactly one backend is configured,
    /// refused with a clear error when 2+ are.
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default = "default_dedup")]
    pub dedup: String,
    #[serde(default)]
    pub worm: bool,
    /// Operator passed `--encrypt`. When `true` the daemon either
    /// mints a fresh AES-256 DEK via the selected keystore backend
    /// (when `key_hex` is absent) or asks the backend to wrap an
    /// operator-supplied key from `key_hex`, then stamps
    /// `manifest.encryption = Some(...)` with the resolved backend
    /// name + wrapped blob. Requires `keystore` to be set.
    #[serde(default)]
    pub encrypt: bool,
    /// Operator-supplied 32-byte AES-256 key, hex-encoded (64
    /// characters). Requires `encrypt: true`; the daemon validates
    /// the length + hex shape and treats a malformed value as a
    /// hard refusal (volume not created). When `None` with
    /// `encrypt: true`, the daemon generates a fresh key.
    #[serde(default)]
    pub key_hex: Option<String>,
    /// Keystore backend name from `keystore.backends:` in the YAML
    /// conffile. Required when `encrypt: true`; `None` triggers
    /// single-backend inference.
    #[serde(default)]
    pub keystore: Option<String>,
    /// `"daemon"` (default) or `"backend"`. Selects whether the DEK
    /// is minted by the daemon's OsRng (then wrapped) or by the
    /// backend's HSM-grade primitive (KMS `GenerateDataKey`, Vault
    /// `transit/datakey/plaintext`). Ignored for the `local`
    /// backend.
    #[serde(default)]
    pub dek_source: Option<String>,
    /// Initial SCSI SYNCHRONIZE CACHE durability tier — `"cloud"`,
    /// `"disk"`, or `"memory"`. `None` (or absent in the request
    /// body) defaults to `cloud` (the safest tier and the
    /// pre-knob behaviour). Mutable later via `POST
    /// /api/v1/volumes/{name}/sync-after`.
    #[serde(default)]
    pub sync_after: Option<String>,
    /// Operator-pinned LUN. `None` auto-assigns the smallest unused
    /// LUN; `Some(N)` reserves the named LUN and refuses with 409
    /// CONFLICT if it's already bound. Pinned LUNs let operators
    /// keep `/dev/disk/by-path/...-lun-N` references stable across
    /// volume add / remove cycles.
    #[serde(default)]
    pub lun: Option<u64>,
}

fn default_page_size_bytes() -> u32 {
    core_block::volume::DEFAULT_PAGE_SIZE_BYTES
}

fn default_dedup() -> String {
    "local".into()
}

/// Wire-format row for `GET /api/v1/volumes` and the create
/// response. Subset of `VolumeManifest` + the runtime LUN binding.
#[derive(Debug, Serialize)]
pub struct VolumeRow {
    pub lun: u64,
    pub name: String,
    pub size_bytes: u64,
    pub sector_bytes: u32,
    pub page_size_bytes: u32,
    pub backend: String,
    pub dedup_scope: &'static str,
    pub worm: bool,
    pub uuid: String,
}

impl VolumeRow {
    fn from_cache(lun: u64, c: &PageCache) -> Self {
        let m = c.manifest();
        Self {
            lun,
            name: m.name.clone(),
            size_bytes: m.size_bytes,
            sector_bytes: m.sector_bytes,
            page_size_bytes: m.page_size_bytes,
            backend: m.backend.clone(),
            dedup_scope: m.dedup_scope.as_str(),
            worm: m.worm,
            uuid: hex::encode(m.uuid),
        }
    }
}
