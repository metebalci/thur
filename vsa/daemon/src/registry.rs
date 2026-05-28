// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! LUN → `PageCache` map consumed by the SCSI dispatcher.
//!
//! thurvsa presents one volume per LUN (no MPIO / multi-path tricks).
//! At boot, [`crate::discovery::discover_and_register`] walks
//! `<data_dir>/volumes/`, sorts alphabetically, and assigns LUNs
//! ascending from 0 — that boot mapping is stable across daemon
//! restarts as long as the volume set doesn't change. The admin
//! socket can `register` / `unregister` at runtime to support live
//! volume create / destroy; live-created volumes get the next free
//! LUN (monotonic), so the alphabetical-sort property only holds
//! for the boot set.
//!
//! Internally the map lives behind a `RwLock` so the SCSI
//! dispatcher's `get` / `luns` reads run concurrently with each
//! other and the admin socket's `register` / `unregister` writes
//! serialise. Per-opcode dispatch holds the read lock for the
//! length of one `BTreeMap::get` (microseconds); the cloned
//! `Arc<PageCache>` is what the async data-path actually uses.
//! `PageCache` wraps the underlying `VolumeWriter` and provides
//! the write-back / RMW layer that lets sub-page WRITE / CAW /
//! UNMAP succeed.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use core_block::PageCache;
use scsi_sbc::VolumeLookup;

/// LUN registry. Built up via [`VolumeRegistry::register`] at boot
/// and mutated at runtime by the admin socket. Methods take
/// `&self` so the wrapper Arc shared with the SCSI dispatcher
/// supports live volume create / destroy without a second handle.
/// (No `Debug` impl — `PageCache` wraps a `VolumeWriter` which
/// holds an `Arc<dyn ObjectStoreBackend>` whose `Debug` print would be
/// noisy in logs.)
#[derive(Default)]
pub struct VolumeRegistry {
    by_lun: RwLock<BTreeMap<u64, Arc<PageCache>>>,
}

impl VolumeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `cache` to `lun`. Returns the previous occupant if
    /// any — caller decides whether to log / panic / proceed; the
    /// boot path treats overwrites as an internal error.
    pub fn register(&self, lun: u64, cache: Arc<PageCache>) -> Option<Arc<PageCache>> {
        let mut map = self.by_lun.write().unwrap_or_else(|p| p.into_inner());
        map.insert(lun, cache)
    }

    /// Look up a LUN. `None` for unmapped LUNs — the dispatcher
    /// converts this into either CHECK CONDITION + LU NOT
    /// SUPPORTED (for opcodes that need a real LU) or the SPC-4
    /// "no-LUN" peripheral-qualifier reply (for INQUIRY).
    pub fn get(&self, lun: u64) -> Option<Arc<PageCache>> {
        let map = self.by_lun.read().unwrap_or_else(|p| p.into_inner());
        map.get(&lun).map(Arc::clone)
    }

    /// Snapshot of LUNs in numeric order. Used by REPORT LUNS
    /// and the admin socket's `GET /api/v1/volumes` listing.
    pub fn luns(&self) -> Vec<u64> {
        let map = self.by_lun.read().unwrap_or_else(|p| p.into_inner());
        map.keys().copied().collect()
    }

    /// Snapshot of `(lun, cache)` pairs. Used by the admin socket
    /// to render the volume list — the cache delegates `manifest()`
    /// to its underlying writer, so this is the cheapest path.
    pub fn entries(&self) -> Vec<(u64, Arc<PageCache>)> {
        let map = self.by_lun.read().unwrap_or_else(|p| p.into_inner());
        map.iter().map(|(lun, w)| (*lun, Arc::clone(w))).collect()
    }

    /// Look up a volume by name. `None` for unknown names — the
    /// upload worker uses this to route an `UploadTask` back to the
    /// owning `VolumeWriter` for `apply_page_upload_outcome`. Linear
    /// scan, same reason as [`Self::unregister_by_name`].
    pub fn get_by_name(&self, name: &str) -> Option<Arc<PageCache>> {
        let map = self.by_lun.read().unwrap_or_else(|p| p.into_inner());
        map.values()
            .find(|c| c.manifest().name == name)
            .map(Arc::clone)
    }

    /// Remove a volume by name. Returns `(lun, cache)` if found,
    /// `None` otherwise. Used by the admin socket's destroy path
    /// before tearing the on-disk volume directory down. Linear
    /// scan — fine at the LUN counts thurvsa targets (hundreds at
    /// most) and avoids a second name → LUN index that has to be
    /// kept in sync.
    pub fn unregister_by_name(&self, name: &str) -> Option<(u64, Arc<PageCache>)> {
        let mut map = self.by_lun.write().unwrap_or_else(|p| p.into_inner());
        let target = map
            .iter()
            .find(|(_, w)| w.manifest().name == name)
            .map(|(lun, _)| *lun)?;
        let cache = map.remove(&target)?;
        Some((target, cache))
    }

    /// Find the next unassigned LUN — smallest `u64` not currently
    /// in the registry. Used by the admin socket to assign LUNs to
    /// live-created volumes.
    pub fn next_free_lun(&self) -> u64 {
        let map = self.by_lun.read().unwrap_or_else(|p| p.into_inner());
        let mut expected: u64 = 0;
        for &lun in map.keys() {
            if lun != expected {
                return expected;
            }
            expected = expected.saturating_add(1);
        }
        expected
    }

    pub fn len(&self) -> usize {
        let map = self.by_lun.read().unwrap_or_else(|p| p.into_inner());
        map.len()
    }

    #[allow(dead_code)] // surfaced for symmetry; only tests + the boot logger touch it today
    pub fn is_empty(&self) -> bool {
        let map = self.by_lun.read().unwrap_or_else(|p| p.into_inner());
        map.is_empty()
    }
}

/// Plug the daemon's registry into the SCSI dispatcher (`scsi-sbc`)
/// without forcing that crate to depend on this one. The dispatcher
/// only needs the read side (LUN → cache + LUN list); mutation
/// stays on the concrete `VolumeRegistry` and remains the admin
/// socket's responsibility.
impl VolumeLookup for VolumeRegistry {
    fn get(&self, lun: u64) -> Option<Arc<PageCache>> {
        VolumeRegistry::get(self, lun)
    }
    fn luns(&self) -> Vec<u64> {
        VolumeRegistry::luns(self)
    }
    fn name_for_lun(&self, lun: u64) -> Option<String> {
        let map = self.by_lun.read().unwrap_or_else(|p| p.into_inner());
        map.get(&lun).map(|c| c.manifest().name.clone())
    }
    fn luns_filtered(&self, allow: Option<&[String]>) -> Vec<u64> {
        let map = self.by_lun.read().unwrap_or_else(|p| p.into_inner());
        match allow {
            None => map.keys().copied().collect(),
            Some(names) => map
                .iter()
                .filter(|(_, c)| names.iter().any(|n| n == &c.manifest().name))
                .map(|(lun, _)| *lun)
                .collect(),
        }
    }
}

/// Same boundary as `VolumeLookup` but for the NVMe/TCP transport.
/// VSA maps `nsid = lun + 1` one-to-one with the SCSI LUN space —
/// NVMe reserves NSID 0 for "no namespace" / broadcast semantics, so
/// LUN 0 (always allocated to the first registered volume) shows up
/// as NSID 1.
impl nvme_nvm::NamespaceLookup for VolumeRegistry {
    fn get(&self, nsid: u32) -> Option<Arc<PageCache>> {
        if nsid == 0 {
            return None;
        }
        VolumeRegistry::get(self, u64::from(nsid - 1))
    }
    fn active_namespaces(&self) -> Vec<u32> {
        VolumeRegistry::luns(self)
            .into_iter()
            .filter_map(|lun| u32::try_from(lun + 1).ok())
            .collect()
    }
    fn name_for_nsid(&self, nsid: u32) -> Option<String> {
        if nsid == 0 {
            return None;
        }
        <Self as VolumeLookup>::name_for_lun(self, u64::from(nsid - 1))
    }
    fn active_namespaces_filtered(&self, allow: Option<&[String]>) -> Vec<u32> {
        <Self as VolumeLookup>::luns_filtered(self, allow)
            .into_iter()
            .filter_map(|lun| u32::try_from(lun + 1).ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_block::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES};
    use core_block::{DedupScope, VolumeManifest, VolumeWriter};
    use shared_object_store::{LocalBackend, ObjectStoreBackend};
    use tempfile::TempDir;

    async fn fixture_cache(name: &str, data_dir: &std::path::Path) -> Arc<PageCache> {
        let cloud_root = data_dir.join("cloud");
        std::fs::create_dir_all(&cloud_root).unwrap();
        let backend = LocalBackend::new(&cloud_root).await.unwrap();
        let backend: Arc<dyn ObjectStoreBackend> = Arc::new(backend);

        VolumeManifest::new(
            name.into(),
            4 * (1u64 << 20),
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            DedupScope::Local,
            false,
            0,
        )
        .unwrap()
        .create(data_dir)
        .unwrap();

        let writer = Arc::new(VolumeWriter::open(data_dir, name, backend).unwrap());
        PageCache::new(writer)
    }

    #[tokio::test]
    async fn register_and_get_round_trip() {
        let tmp = TempDir::new().unwrap();
        let reg = VolumeRegistry::new();
        let w = fixture_cache("vol1", tmp.path()).await;
        assert!(reg.register(0, Arc::clone(&w)).is_none());
        assert!(reg.get(0).is_some());
        assert!(reg.get(1).is_none());
        assert_eq!(reg.len(), 1);
    }

    #[tokio::test]
    async fn luns_returns_sorted_lun_ids() {
        let tmp = TempDir::new().unwrap();
        let reg = VolumeRegistry::new();
        let a = fixture_cache("vola", tmp.path()).await;
        let b = fixture_cache("volb", tmp.path()).await;
        reg.register(2, a);
        reg.register(0, b);
        assert_eq!(reg.luns(), vec![0, 2]);
    }

    #[tokio::test]
    async fn next_free_lun_picks_smallest_gap() {
        let tmp = TempDir::new().unwrap();
        let reg = VolumeRegistry::new();
        assert_eq!(reg.next_free_lun(), 0);
        reg.register(0, fixture_cache("a", tmp.path()).await);
        assert_eq!(reg.next_free_lun(), 1);
        reg.register(2, fixture_cache("c", tmp.path()).await);
        // Gap at 1.
        assert_eq!(reg.next_free_lun(), 1);
        reg.register(1, fixture_cache("b", tmp.path()).await);
        assert_eq!(reg.next_free_lun(), 3);
    }

    #[tokio::test]
    async fn unregister_by_name_returns_lun_and_cache() {
        let tmp = TempDir::new().unwrap();
        let reg = VolumeRegistry::new();
        let a = fixture_cache("alpha", tmp.path()).await;
        let b = fixture_cache("beta", tmp.path()).await;
        reg.register(0, a);
        reg.register(1, b);

        let (lun, _cache) = reg.unregister_by_name("alpha").expect("found");
        assert_eq!(lun, 0);
        assert!(reg.get(0).is_none());
        assert!(reg.get(1).is_some());
        assert_eq!(reg.len(), 1);

        // Second unregister of the same name → None.
        assert!(reg.unregister_by_name("alpha").is_none());
    }

    #[tokio::test]
    async fn name_for_lun_resolves_and_misses() {
        let tmp = TempDir::new().unwrap();
        let reg = VolumeRegistry::new();
        reg.register(0, fixture_cache("alpha", tmp.path()).await);
        reg.register(1, fixture_cache("beta", tmp.path()).await);

        assert_eq!(
            <VolumeRegistry as VolumeLookup>::name_for_lun(&reg, 0).as_deref(),
            Some("alpha")
        );
        assert_eq!(
            <VolumeRegistry as VolumeLookup>::name_for_lun(&reg, 1).as_deref(),
            Some("beta")
        );
        assert!(<VolumeRegistry as VolumeLookup>::name_for_lun(&reg, 99).is_none());
    }

    #[tokio::test]
    async fn luns_filtered_with_no_fence_returns_all() {
        let tmp = TempDir::new().unwrap();
        let reg = VolumeRegistry::new();
        reg.register(0, fixture_cache("alpha", tmp.path()).await);
        reg.register(1, fixture_cache("beta", tmp.path()).await);

        assert_eq!(
            <VolumeRegistry as VolumeLookup>::luns_filtered(&reg, None),
            vec![0, 1]
        );
    }

    #[tokio::test]
    async fn luns_filtered_excludes_unadmitted_volumes() {
        let tmp = TempDir::new().unwrap();
        let reg = VolumeRegistry::new();
        reg.register(0, fixture_cache("alpha", tmp.path()).await);
        reg.register(1, fixture_cache("beta", tmp.path()).await);
        reg.register(2, fixture_cache("gamma", tmp.path()).await);

        // Only beta is admitted — alpha and gamma must drop out.
        let allow = vec!["beta".to_string()];
        assert_eq!(
            <VolumeRegistry as VolumeLookup>::luns_filtered(&reg, Some(&allow)),
            vec![1]
        );

        // Admitting an unknown name yields an empty list — there's
        // nothing to show.
        let allow = vec!["nonexistent".to_string()];
        assert!(<VolumeRegistry as VolumeLookup>::luns_filtered(&reg, Some(&allow)).is_empty());
    }

    #[tokio::test]
    async fn active_namespaces_filtered_maps_lun_plus_one() {
        use nvme_nvm::NamespaceLookup;
        let tmp = TempDir::new().unwrap();
        let reg = VolumeRegistry::new();
        reg.register(0, fixture_cache("alpha", tmp.path()).await);
        reg.register(1, fixture_cache("beta", tmp.path()).await);

        // No fence → all NSIDs (nsid = lun + 1).
        assert_eq!(
            <VolumeRegistry as NamespaceLookup>::active_namespaces_filtered(&reg, None),
            vec![1, 2]
        );

        // Fenced to alpha only → NSID 1.
        let allow = vec!["alpha".to_string()];
        assert_eq!(
            <VolumeRegistry as NamespaceLookup>::active_namespaces_filtered(&reg, Some(&allow)),
            vec![1]
        );

        // NSID 0 is never resolved (reserved by NVMe).
        assert!(<VolumeRegistry as NamespaceLookup>::name_for_nsid(&reg, 0).is_none());
    }
}
