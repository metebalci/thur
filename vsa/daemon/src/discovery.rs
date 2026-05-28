// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Volume discovery + LUN assignment + backend instantiation.
//!
//! Boot path: walk `<data_dir>/volumes/`, group by `manifest.backend`,
//! instantiate one `Arc<dyn ObjectStoreBackend>` per unique backend name (so
//! N volumes pointing at the same backend share one client + cred
//! material), and bind each volume to a deterministic LUN — sorted
//! ascending by volume name, starting at 0. Operator-pinned LUNs are
//! a future feature; for now the daemon picks up volume changes on
//! restart, so a stable name → LUN mapping survives daemon bounces
//! as long as the volume set doesn't change.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use core_block::{self, PageCache, UploadTask, VolumeManifest, VolumeWriter};
use shared_keystore::{KeyStoreBackend, KeystoreYamlConfig};
use shared_object_store::{ObjectStoreBackend, ObjectStoreConfig};
use shared_pool::PoolBudget;
use tokio::sync::mpsc;

use crate::registry::VolumeRegistry;

/// Per-volume summary captured during discovery — surfaced to the
/// caller (today, the boot logger) so operators see the LUN map at
/// startup. Carries enough context to render a "LUN N: name=…
/// backend=… size=… page=…" line without re-reading the manifest.
pub struct DiscoveredVolume {
    pub lun: u64,
    pub name: String,
    pub backend: String,
    pub size_bytes: u64,
    pub page_size_bytes: u32,
}

/// Walk `<data_dir>/volumes/`, build a [`VolumeRegistry`], and return
/// it alongside a per-volume summary list (LUN order), the per-
/// volume [`PageCache`] handles (so the daemon can spawn flush
/// workers), and the cache of `Arc<dyn ObjectStoreBackend>` instances
/// built up during discovery. Returns an empty registry + empty
/// summary list when no volumes exist — the daemon can still boot
/// to wait for the first `thurvsa volume create`. The backend
/// cache is exposed so the admin socket can reuse already-
/// authenticated client objects when servicing live
/// `POST /api/v1/volumes` against backends already in use.
#[allow(clippy::type_complexity)]
#[allow(clippy::too_many_arguments)]
pub async fn discover_and_register(
    data_dir: &Path,
    cloud: &ObjectStoreConfig,
    keystore_config: &KeystoreYamlConfig,
    max_concurrent_flushes: usize,
    pool_budgets: &HashMap<String, Arc<PoolBudget>>,
    ghost_lists: &HashMap<String, Arc<shared_pool::GhostList>>,
    backpressure_deadline: Duration,
    upload_sender: Option<mpsc::Sender<UploadTask>>,
) -> Result<(
    VolumeRegistry,
    Vec<DiscoveredVolume>,
    Vec<Arc<PageCache>>,
    BTreeMap<String, Arc<dyn ObjectStoreBackend>>,
    BTreeMap<String, Arc<dyn KeyStoreBackend>>,
)> {
    let names = VolumeManifest::list(data_dir)
        .with_context(|| format!("list volumes under {}", data_dir.display()))?;

    let mut manifests: Vec<VolumeManifest> = Vec::with_capacity(names.len());
    for name in &names {
        let m = VolumeManifest::load(data_dir, name)
            .with_context(|| format!("load manifest for volume '{name}'"))?;
        manifests.push(m);
    }
    // Sort by name for deterministic sentinel-LUN resolution order.
    // Post-v4, every manifest carries its own pinned `lun` field and
    // this order no longer drives the LUN map — it only matters for
    // pre-v4 manifests that still have `UNASSIGNED_LUN` and need a
    // one-shot migration.
    manifests.sort_by(|a, b| a.name.cmp(&b.name));

    // Resolve sentinel LUNs to real values (smallest unused), persist
    // the migrated manifest back to disk so the next boot is steady-
    // state, and refuse to start on any real LUN collision. The
    // assignment must be deterministic: walk in alphabetical order,
    // pick the smallest unused for each sentinel — matches the
    // historical "name-sort starting at 0" behavior for pre-v4
    // upgrades, so existing operators see no LUN movement.
    use core_block::volume::UNASSIGNED_LUN;
    let mut used: std::collections::BTreeSet<u64> = manifests
        .iter()
        .filter_map(|m| (m.lun != UNASSIGNED_LUN).then_some(m.lun))
        .collect();
    for m in manifests.iter_mut() {
        if m.lun != UNASSIGNED_LUN {
            continue;
        }
        let mut expected: u64 = 0;
        for &lun in &used {
            if lun != expected {
                break;
            }
            expected = expected.saturating_add(1);
        }
        m.lun = expected;
        used.insert(expected);
        let vol_dir = VolumeManifest::dir_for(data_dir, &m.name);
        m.persist(&vol_dir).with_context(|| {
            format!(
                "persist auto-assigned LUN {} for pre-v4 volume '{}'",
                m.lun, m.name,
            )
        })?;
        tracing::info!(
            "volume '{}': auto-assigned LUN {} (pre-v4 manifest migrated)",
            m.name,
            m.lun
        );
    }
    {
        let mut seen: std::collections::HashMap<u64, &str> =
            std::collections::HashMap::with_capacity(manifests.len());
        for m in &manifests {
            if let Some(prev) = seen.insert(m.lun, m.name.as_str()) {
                return Err(anyhow!(
                    "manifest LUN collision: volumes '{}' and '{}' both claim LUN {}; \
                     edit one manifest to a unique LUN before starting the daemon",
                    prev,
                    m.name,
                    m.lun,
                ));
            }
        }
    }

    // One ObjectStoreBackend per unique manifest.backend. Instantiation
    // reads creds and (for non-Local) issues an SDK construction
    // call, so we do it once per name.
    let mut backends: BTreeMap<String, Arc<dyn ObjectStoreBackend>> = BTreeMap::new();
    for m in &manifests {
        if backends.contains_key(&m.backend) {
            continue;
        }
        if !cloud.backends.contains_key(&m.backend) {
            return Err(anyhow!(
                "volume '{}' references storage backend '{}' which is not defined under `cloud.backends:` in the YAML conffile",
                m.name,
                m.backend
            ));
        }
        let backend_box = cloud
            .create_backend_named(&m.backend)
            .await
            .with_context(|| {
                format!(
                    "instantiate storage backend '{}' (referenced by volume '{}')",
                    m.backend, m.name
                )
            })?;
        let backend_arc: Arc<dyn ObjectStoreBackend> = Arc::from(backend_box);
        // Cache warmup: LIST chunks/ once and seed every key as
        // `Probed` so subsequent chunk_exists / upload_chunk hit the
        // cache. Non-blocking — a LIST failure leaves the cache cold;
        // next write does a real HEAD/PUT (same as pre-cache behaviour).
        let warmup_arc: Arc<dyn ObjectStoreBackend> = Arc::clone(&backend_arc);
        let warmup_name = m.backend.clone();
        tokio::spawn(async move {
            match warmup_arc.warmup_prefix("chunks/").await {
                Ok(n) => tracing::info!(
                    "cloud cache warmup: seeded {} chunks/ keys for backend '{}'",
                    n,
                    warmup_name
                ),
                Err(e) => tracing::warn!(
                    "cloud cache warmup failed for backend '{}': {} (continuing with cold cache)",
                    warmup_name,
                    e
                ),
            }
        });
        backends.insert(m.backend.clone(), backend_arc);
    }

    // One KeyStoreBackend per unique encryption.keystore_backend
    // referenced in the discovered manifests. Same shape as the
    // cloud-backend cache above; mounted into AdminState so admin
    // handlers re-use the authenticated KMS / Vault clients.
    let mut keystores: BTreeMap<String, Arc<dyn KeyStoreBackend>> = BTreeMap::new();
    for m in &manifests {
        let Some(enc) = m.encryption.as_ref() else {
            continue;
        };
        let name = enc.keystore_backend.as_str();
        if keystores.contains_key(name) {
            continue;
        }
        if !keystore_config.backends.contains_key(name) {
            return Err(anyhow!(
                "volume '{}' references keystore backend '{}' which is not defined under `keystore.backends:` in the YAML conffile",
                m.name,
                name
            ));
        }
        let boxed = keystore_config
            .create_backend_named(name, data_dir)
            .await
            .with_context(|| {
                format!(
                    "instantiate keystore backend '{}' (referenced by volume '{}')",
                    name, m.name
                )
            })?;
        keystores.insert(name.to_string(), Arc::from(boxed));
    }

    let registry = VolumeRegistry::new();
    let mut summaries = Vec::with_capacity(manifests.len());
    let mut caches = Vec::with_capacity(manifests.len());
    for m in manifests.into_iter() {
        let lun = m.lun;
        let backend = backends
            .get(&m.backend)
            .cloned()
            .ok_or_else(|| anyhow!("internal: backend '{}' not staged", m.backend))?;
        // Encrypted volumes ask the named keystore backend to
        // unwrap their DEK. `local` reads the on-disk sidecar; KMS
        // / Vault decrypt the manifest-stamped wrapped blob.
        // Plaintext volumes take the original `open` path.
        // Per-backend chunk-pool budget. Empty when the daemon
        // hasn't built a budget for this backend (e.g. a test
        // harness that constructs `discover_and_register` with
        // `HashMap::new()` for `pool_budgets`); fall back to an
        // unbounded budget so the writer still works.
        let pool_budget = pool_budgets
            .get(&m.backend)
            .cloned()
            .unwrap_or_else(|| Arc::new(PoolBudget::unbounded(data_dir.to_path_buf())));
        let ghost_list = ghost_lists.get(&m.backend).cloned();
        let writer = if let Some(enc) = m.encryption.as_ref() {
            let ks = keystores
                .get(&enc.keystore_backend)
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "internal: keystore '{}' not staged (volume '{}')",
                        enc.keystore_backend,
                        m.name
                    )
                })?;
            let wrapped: Vec<u8> = match enc.wrapped_dek.as_deref() {
                Some(b64) => base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .with_context(|| {
                        format!(
                            "decode wrapped_dek for '{}' (keystore={})",
                            m.name, enc.keystore_backend
                        )
                    })?,
                None => Vec::new(),
            };
            let secret = ks.unwrap(&m.uuid, &wrapped).await.with_context(|| {
                format!(
                    "unwrap DEK for volume '{}' via keystore '{}'",
                    m.name, enc.keystore_backend
                )
            })?;
            let mut w = VolumeWriter::open_with_key(data_dir, &m.name, backend, *secret.as_bytes())
                .with_context(|| format!("open encrypted volume writer for '{}'", m.name))?
                .with_pool_budget(pool_budget, backpressure_deadline);
            if let Some(gl) = ghost_list.clone() {
                w = w.with_ghost_list(gl);
            }
            if let Some(s) = upload_sender.clone() {
                w = w.with_upload_sender(s);
            }
            Arc::new(w)
        } else {
            let mut w = VolumeWriter::open(data_dir, &m.name, backend)
                .with_context(|| format!("open volume writer for '{}'", m.name))?
                .with_pool_budget(pool_budget, backpressure_deadline);
            if let Some(gl) = ghost_list.clone() {
                w = w.with_ghost_list(gl);
            }
            if let Some(s) = upload_sender.clone() {
                w = w.with_upload_sender(s);
            }
            Arc::new(w)
        };
        let cache = PageCache::with_budget_and_concurrency(
            writer,
            core_block::DEFAULT_CACHE_BUDGET_BYTES,
            max_concurrent_flushes,
        );
        let summary = DiscoveredVolume {
            lun,
            name: m.name.clone(),
            backend: m.backend.clone(),
            size_bytes: m.size_bytes,
            page_size_bytes: m.page_size_bytes,
        };
        if registry.register(lun, Arc::clone(&cache)).is_some() {
            return Err(anyhow!(
                "internal error: LUN {lun} double-assigned (volume '{}')",
                m.name
            ));
        }
        summaries.push(summary);
        caches.push(cache);
    }

    Ok((registry, summaries, caches, backends, keystores))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_block::DedupScope;
    use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn local_storage_config(root: &Path) -> ObjectStoreConfig {
        let yaml = format!(
            r#"
backends:
  devbox:
    type: local
    root_dir: {}
"#,
            root.display()
        );
        let cfg: ObjectStoreConfig =
            serde_yaml::from_str(&yaml).expect("parse local ObjectStoreConfig");
        cfg.validate_backends().expect("validate");
        cfg
    }

    fn local_keystore_backends() -> KeystoreYamlConfig {
        let mut backends = std::collections::BTreeMap::new();
        backends.insert(
            "local".to_string(),
            shared_keystore::KeystoreBackendEntry::Local(
                shared_keystore::LocalBackendConfig::default(),
            ),
        );
        KeystoreYamlConfig { backends }
    }

    fn create_volume(data_dir: &Path, name: &str, backend: &str) {
        // Pre-v4 simulation: manifests on disk have the sentinel LUN
        // so the discovery layer exercises the auto-assign-and-persist
        // migration path. Real callers (admin handler, CLI) pre-pick
        // the LUN before calling `new`.
        VolumeManifest::new(
            name.to_string(),
            4 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            backend.to_string(),
            DedupScope::Local,
            false,
            core_block::volume::UNASSIGNED_LUN,
        )
        .expect("manifest new")
        .create(data_dir)
        .expect("manifest create");
    }

    #[tokio::test]
    async fn empty_data_dir_returns_empty_registry() {
        let tmp = TempDir::new().expect("tempdir");
        let cloud_root = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud_root).expect("mkdir cloud");
        let cfg = local_storage_config(&cloud_root);

        let (reg, vols, _caches, _backends, _keystores) = discover_and_register(
            tmp.path(),
            &cfg,
            &local_keystore_backends(),
            1,
            &HashMap::new(),
            &HashMap::new(),
            Duration::from_secs(30),
            None,
        )
        .await
        .expect("discover");
        assert!(reg.is_empty());
        assert!(vols.is_empty());
    }

    #[tokio::test]
    async fn assigns_luns_in_alphabetical_order() {
        let tmp = TempDir::new().expect("tempdir");
        let cloud_root = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud_root).expect("mkdir cloud");
        let cfg = local_storage_config(&cloud_root);

        create_volume(tmp.path(), "zeta", "devbox");
        create_volume(tmp.path(), "alpha", "devbox");
        create_volume(tmp.path(), "mid", "devbox");

        let (reg, vols, _caches, _backends, _keystores) = discover_and_register(
            tmp.path(),
            &cfg,
            &local_keystore_backends(),
            1,
            &HashMap::new(),
            &HashMap::new(),
            Duration::from_secs(30),
            None,
        )
        .await
        .expect("discover");
        assert_eq!(reg.len(), 3);
        let by_lun: BTreeMap<u64, &str> = vols.iter().map(|v| (v.lun, v.name.as_str())).collect();
        assert_eq!(by_lun.get(&0).copied(), Some("alpha"));
        assert_eq!(by_lun.get(&1).copied(), Some("mid"));
        assert_eq!(by_lun.get(&2).copied(), Some("zeta"));
    }

    #[tokio::test]
    async fn dispatches_via_handler_after_discovery() {
        // End-to-end smoke: discovery → registry → SCSI handler →
        // INQUIRY against the discovered LUN comes back GOOD.
        let tmp = TempDir::new().expect("tempdir");
        let cloud_root = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud_root).expect("mkdir cloud");
        let cfg = local_storage_config(&cloud_root);
        create_volume(tmp.path(), "vol1", "devbox");

        let (registry, vols, _caches, _backends, _keystores) = discover_and_register(
            tmp.path(),
            &cfg,
            &local_keystore_backends(),
            1,
            &HashMap::new(),
            &HashMap::new(),
            Duration::from_secs(30),
            None,
        )
        .await
        .expect("discover");
        assert_eq!(vols.len(), 1);
        assert_eq!(vols[0].lun, 0);

        let handler = scsi_sbc::SbcScsiDispatcher::new(
            Arc::new(registry) as Arc<dyn scsi_sbc::VolumeLookup>,
            scsi_sbc::ISCSI_DISK_TARGET_IQN.to_string(),
        );
        let cdb = [0x12u8, 0, 0, 0x00, 0x60, 0];
        let resp = handler
            .dispatch(scsi_spc::scsi::ScsiRequest {
                lun: 0,
                cdb: &cdb,
                data_out: &[],
                data_in_max: 4096,
                tsih: 0,
                initiator_iqn: None,
                cid: 0,
                peer: "",
                session_partition: None,
                session_volumes: None,
            })
            .await;
        assert!(resp.sense.is_none(), "{:?}", resp.sense);
        // Vendor "MB      " at offset 8.
        assert_eq!(&resp.data_in[8..16], b"MB      ");
    }

    #[tokio::test]
    async fn unknown_backend_in_manifest_is_rejected() {
        let tmp = TempDir::new().expect("tempdir");
        let cloud_root = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud_root).expect("mkdir cloud");
        let cfg = local_storage_config(&cloud_root);

        // Volume points at a backend name not in thurvsa.yaml.
        create_volume(tmp.path(), "vol1", "ghost");

        // VolumeRegistry deliberately has no Debug impl, so `expect_err`
        // can't print its (Ok) shape — match the Result manually.
        match discover_and_register(
            tmp.path(),
            &cfg,
            &local_keystore_backends(),
            1,
            &HashMap::new(),
            &HashMap::new(),
            Duration::from_secs(30),
            None,
        )
        .await
        {
            Ok(_) => panic!("must reject unknown backend"),
            Err(err) => {
                let msg = format!("{err:#}");
                assert!(
                    msg.contains("ghost") && msg.contains("not defined"),
                    "unexpected error: {msg}"
                );
            }
        }
    }

    /// Build a v4 manifest with an explicit (already-assigned) LUN —
    /// bypasses the create_volume helper which always stamps the
    /// pre-v4 sentinel. Used by the collision test to set up two
    /// manifests claiming the same LUN.
    fn create_volume_with_lun(data_dir: &Path, name: &str, backend: &str, lun: u64) {
        VolumeManifest::new(
            name.to_string(),
            4 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            backend.to_string(),
            DedupScope::Local,
            false,
            lun,
        )
        .expect("manifest new")
        .create(data_dir)
        .expect("manifest create");
    }

    #[tokio::test]
    async fn explicit_lun_collision_is_rejected_with_both_names() {
        // Two manifests carrying the same explicit LUN must fail
        // discovery with both names + the LUN in the error message,
        // so the operator can pick which one to renumber.
        let tmp = TempDir::new().expect("tempdir");
        let cloud_root = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud_root).expect("mkdir cloud");
        let cfg = local_storage_config(&cloud_root);

        create_volume_with_lun(tmp.path(), "alpha", "devbox", 5);
        create_volume_with_lun(tmp.path(), "beta", "devbox", 5);

        match discover_and_register(
            tmp.path(),
            &cfg,
            &local_keystore_backends(),
            1,
            &HashMap::new(),
            &HashMap::new(),
            Duration::from_secs(30),
            None,
        )
        .await
        {
            Ok(_) => panic!("must reject LUN collision"),
            Err(err) => {
                let msg = format!("{err:#}");
                assert!(msg.contains("LUN collision"), "no collision phrase: {msg}");
                assert!(msg.contains("alpha"), "alpha not named: {msg}");
                assert!(msg.contains("beta"), "beta not named: {msg}");
                assert!(msg.contains('5'), "LUN number not named: {msg}");
            }
        }
    }

    #[tokio::test]
    async fn pre_v4_migration_fills_gap_left_by_explicit_lun() {
        // Mixed v4-stamped + pre-v4 sentinel manifests: the sentinels
        // must be auto-assigned to the smallest free LUN, slotting
        // around the explicit one. Proves the migration path doesn't
        // collide with already-pinned LUNs.
        let tmp = TempDir::new().expect("tempdir");
        let cloud_root = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud_root).expect("mkdir cloud");
        let cfg = local_storage_config(&cloud_root);

        create_volume_with_lun(tmp.path(), "pinned", "devbox", 0);
        create_volume(tmp.path(), "auto_a", "devbox");
        create_volume(tmp.path(), "auto_b", "devbox");

        let (_reg, vols, _caches, _backends, _keystores) = discover_and_register(
            tmp.path(),
            &cfg,
            &local_keystore_backends(),
            1,
            &HashMap::new(),
            &HashMap::new(),
            Duration::from_secs(30),
            None,
        )
        .await
        .expect("discover");
        let by_name: HashMap<&str, u64> = vols.iter().map(|v| (v.name.as_str(), v.lun)).collect();
        assert_eq!(by_name.get("pinned").copied(), Some(0));
        // auto_a precedes auto_b alphabetically; sentinels migrate
        // to the next free LUNs starting at 1.
        assert_eq!(by_name.get("auto_a").copied(), Some(1));
        assert_eq!(by_name.get("auto_b").copied(), Some(2));
    }

    #[tokio::test]
    async fn shares_one_backend_across_multiple_volumes() {
        // Two volumes pointing at the same backend should not blow
        // the daemon up — and discovery should succeed end-to-end.
        let tmp = TempDir::new().expect("tempdir");
        let cloud_root = tmp.path().join("cloud");
        std::fs::create_dir_all(&cloud_root).expect("mkdir cloud");
        let cfg = local_storage_config(&cloud_root);
        create_volume(tmp.path(), "a", "devbox");
        create_volume(tmp.path(), "b", "devbox");

        let (reg, vols, _caches, _backends, _keystores) = discover_and_register(
            tmp.path(),
            &cfg,
            &local_keystore_backends(),
            1,
            &HashMap::new(),
            &HashMap::new(),
            Duration::from_secs(30),
            None,
        )
        .await
        .expect("discover");
        assert_eq!(reg.len(), 2);
        assert_eq!(vols.len(), 2);
        for v in &vols {
            assert_eq!(v.backend, "devbox");
        }
    }
}
