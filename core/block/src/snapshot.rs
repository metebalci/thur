// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Snapshot manifest — a frozen point-in-time copy of a volume's page
//! table, persisted at
//! `<data_dir>/volumes/<parent>/snapshots/<snap>/snap.json` alongside a
//! byte-for-byte copy of the parent's `pages.idx` at snapshot time
//! (issue #13).
//!
//! ## Why this is enough for copy-on-write
//!
//! The VSA write path already does copy-on-write at the chunk level: a
//! WRITE seals a new content-addressed chunk, repoints the page-table
//! entry ([`crate::page_index::PageIndex::set_unsynced`]), and leaves
//! the old chunk for `system gc` to reclaim. Chunks carry no on-disk
//! refcount — a chunk is alive iff some `pages.idx` references it, which
//! `gc.rs` enforces by walking every volume's index. So a snapshot only
//! needs to *retain* a frozen copy of the page table: GC then sees the
//! old chunk as still-referenced and keeps it, while the parent's live
//! index moves on. No hot-path change is required.
//!
//! ## Identity and the pool namespace
//!
//! The copied `pages.idx` header binds it to the *parent's* uuid (bytes
//! 16..32), so a snapshot stores `uuid = parent.uuid` and the index
//! opens with no rewrite. Chunk resolution, however, keys on the family
//! [`dedup_namespace`](VolumeManifest::dedup_namespace_uuid): under
//! `Local` dedup the parent's chunks live under
//! `<backend>/<namespace_from_uuid(family)>/`, so the snapshot records
//! that family namespace and resolves there. Under `Global` dedup the
//! pool is shared and the namespace is `None`.
//!
//! ## Consistency
//!
//! Snapshot create quiesces the parent (flush dirty pages to the pool,
//! await cloud acks, fdatasync the index) before copying — so the
//! snapshot is crash-consistent up to the snapshot point. Application
//! consistency is the host's job (fsync / fs-freeze before snapshot),
//! the standard array-side contract.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::volume::{
    DedupScope, VolumeEncryptionMeta, VolumeError, VolumeManifest, namespace_from_uuid,
    validate_name,
};

/// Current on-disk snapshot-manifest version.
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// Inline serde for a 16-byte UUID as lowercase hex — matching the
/// volume manifest's encoding so a snapshot's `uuid` / `parent_uuid` /
/// `dedup_namespace` read the same way in both files.
mod uuid_hex {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(uuid: &[u8; 16], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(uuid))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 16], D::Error> {
        let s = String::deserialize(d)?;
        let bytes = hex::decode(&s).map_err(D::Error::custom)?;
        if bytes.len() != 16 {
            return Err(D::Error::custom("uuid must be 16 bytes"));
        }
        let mut out = [0u8; 16];
        out.copy_from_slice(&bytes);
        Ok(out)
    }
}

/// Persistent snapshot metadata. Carries everything `system gc`,
/// eviction, and clone-create need to resolve the frozen `pages.idx`
/// against the right pool without re-reading the parent manifest (which
/// may have been destroyed — the snapshot outlives it until explicitly
/// removed).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SnapshotManifest {
    pub schema_version: u32,
    /// Operator-chosen snapshot name. Unique within the parent volume's
    /// `snapshots/` directory; two different volumes may both have a
    /// `daily` snapshot.
    pub name: String,
    /// The parent's uuid at snapshot time — the copied `pages.idx`
    /// header is bound to this, so it must match for the index to open.
    #[serde(with = "uuid_hex")]
    pub uuid: [u8; 16],
    /// Parent volume name at snapshot time (for display / listing).
    pub parent_volume: String,
    /// Parent uuid (== `uuid`; kept explicit for clarity in the file).
    #[serde(with = "uuid_hex")]
    pub parent_uuid: [u8; 16],
    pub created_at: DateTime<Utc>,
    /// Cloud backend the parent's chunks live under.
    pub backend: String,
    pub dedup_scope: DedupScope,
    /// The family chunk-pool namespace (see module docs). Equal to the
    /// parent's [`VolumeManifest::dedup_namespace_uuid`].
    #[serde(with = "uuid_hex")]
    pub dedup_namespace: [u8; 16],
    /// Page size — needed to open the copied `pages.idx`.
    pub page_size_bytes: u32,
    /// SBC-3 logical block size — carried so a clone made from this
    /// snapshot comes up with the parent's sector size.
    pub sector_bytes: u32,
    /// Logical (host-visible) size captured at snapshot time — the
    /// parent's *live* size (post-resize), so a clone made from this
    /// snapshot comes up at the right capacity.
    pub size_bytes: u64,
    /// At-rest encryption metadata copied from the parent. The snapshot
    /// itself never decrypts (it is not host-visible); this records
    /// whether the underlying chunks are encrypted so clone-create can
    /// refuse an encrypted source (issue #86).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<VolumeEncryptionMeta>,
}

impl SnapshotManifest {
    /// Filename for the on-disk snapshot manifest.
    pub const FILENAME: &'static str = "snap.json";

    /// Subdirectory under a volume directory that holds its snapshots.
    pub const SNAPSHOTS_SUBDIR: &'static str = "snapshots";

    /// `<data_dir>/volumes/<parent>/snapshots/`.
    pub fn snapshots_dir(data_dir: &Path, parent_volume: &str) -> PathBuf {
        VolumeManifest::dir_for(data_dir, parent_volume).join(Self::SNAPSHOTS_SUBDIR)
    }

    /// `<data_dir>/volumes/<parent>/snapshots/<snap>/`.
    pub fn dir_for(data_dir: &Path, parent_volume: &str, snap: &str) -> PathBuf {
        Self::snapshots_dir(data_dir, parent_volume).join(snap)
    }

    /// `<data_dir>/volumes/<parent>/snapshots/<snap>/snap.json`.
    pub fn path_for(data_dir: &Path, parent_volume: &str, snap: &str) -> PathBuf {
        Self::dir_for(data_dir, parent_volume, snap).join(Self::FILENAME)
    }

    /// Resolve the copied `pages.idx` path for a snapshot.
    pub fn page_index_path(data_dir: &Path, parent_volume: &str, snap: &str) -> PathBuf {
        crate::page_index::PageIndex::path_for(&Self::dir_for(data_dir, parent_volume, snap))
    }

    /// Build a snapshot manifest from the parent volume manifest and the
    /// parent's *live* size. Validates the snapshot name. Does not touch
    /// disk — the daemon creates the directory, copies the frozen
    /// `pages.idx`, then calls [`Self::persist`].
    pub fn new(
        name: String,
        parent: &VolumeManifest,
        live_size_bytes: u64,
    ) -> Result<Self, VolumeError> {
        validate_name(&name)?;
        Ok(Self {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            name,
            uuid: parent.uuid,
            parent_volume: parent.name.clone(),
            parent_uuid: parent.uuid,
            created_at: Utc::now(),
            backend: parent.backend.clone(),
            dedup_scope: parent.dedup_scope,
            dedup_namespace: parent.dedup_namespace_uuid(),
            page_size_bytes: parent.page_size_bytes,
            sector_bytes: parent.sector_bytes,
            size_bytes: live_size_bytes,
            encryption: parent.encryption.clone(),
        })
    }

    /// Chunk-pool namespace for this snapshot's frozen index: `None`
    /// under `Global` dedup, `Some(hex-of-family-uuid)` under `Local`.
    /// Keyed on the family `dedup_namespace`, so the snapshot's hashes
    /// bucket alongside the parent's and any clones' in the GC live set.
    pub fn pool_namespace(&self) -> Option<String> {
        match self.dedup_scope {
            DedupScope::Global => None,
            DedupScope::Local => Some(namespace_from_uuid(&self.dedup_namespace)),
        }
    }

    /// Atomic write: tmp + fsync + rename, matching
    /// [`VolumeManifest::persist`]. The rename is the snapshot's commit
    /// point — a crash before it leaves a stray `pages.idx` with no
    /// `snap.json`, which every walker (discovery / GC / eviction)
    /// skips.
    pub fn persist(&self, snap_dir: &Path) -> Result<(), VolumeError> {
        let tmp = snap_dir.join("snap.json.tmp");
        let final_path = snap_dir.join(Self::FILENAME);
        {
            let f = fs::File::create(&tmp)?;
            let mut w = BufWriter::new(f);
            serde_json::to_writer(&mut w, self)?;
            w.flush()?;
            w.into_inner()
                .map_err(|e| std::io::Error::other(e.to_string()))?
                .sync_all()?;
        }
        fs::rename(tmp, final_path)?;
        Ok(())
    }

    /// Load a snapshot manifest. Stamps the in-memory version forward,
    /// same as [`VolumeManifest::load`].
    pub fn load(data_dir: &Path, parent_volume: &str, snap: &str) -> Result<Self, VolumeError> {
        let path = Self::path_for(data_dir, parent_volume, snap);
        if !path.exists() {
            return Err(VolumeError::NotFound(path));
        }
        let raw = fs::read_to_string(&path)?;
        let mut manifest: Self = serde_json::from_str(&raw)?;
        if manifest.schema_version == 0 || manifest.schema_version > SNAPSHOT_SCHEMA_VERSION {
            return Err(VolumeError::SchemaMismatch {
                found: manifest.schema_version,
                expected: SNAPSHOT_SCHEMA_VERSION,
            });
        }
        manifest.schema_version = SNAPSHOT_SCHEMA_VERSION;
        Ok(manifest)
    }

    /// Enumerate snapshot names for one volume, sorted. Empty when the
    /// volume has no `snapshots/` directory yet.
    pub fn list(data_dir: &Path, parent_volume: &str) -> Result<Vec<String>, VolumeError> {
        let dir = Self::snapshots_dir(data_dir, parent_volume);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Some(n) = entry.file_name().to_str()
            {
                names.push(n.to_string());
            }
        }
        names.sort();
        Ok(names)
    }

    /// Enumerate every `(parent_volume, snapshot)` pair across all
    /// volumes, sorted by `(parent, snapshot)`. The cross-cutting walk
    /// `system gc` and eviction use to fold snapshot page indexes into
    /// the live set. Skips volumes with no snapshots.
    pub fn list_all(data_dir: &Path) -> Result<Vec<(String, String)>, VolumeError> {
        let mut out = Vec::new();
        for parent in VolumeManifest::list(data_dir)? {
            for snap in Self::list(data_dir, &parent)? {
                out.push((parent.clone(), snap));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::{DEFAULT_PAGE_SIZE_BYTES, DEFAULT_SECTOR_BYTES, VolumeEncryptionAlgorithm};
    use tempfile::TempDir;

    fn parent(name: &str, scope: DedupScope) -> VolumeManifest {
        VolumeManifest::new(
            name.into(),
            1u64 << 30,
            DEFAULT_SECTOR_BYTES,
            DEFAULT_PAGE_SIZE_BYTES,
            "primary".into(),
            scope,
            false,
            0,
        )
        .unwrap()
    }

    #[test]
    fn snapshot_inherits_identity_and_namespace() {
        let p = parent("vol1", DedupScope::Local);
        let s = SnapshotManifest::new("snap1".into(), &p, 2u64 << 30).unwrap();
        // Identity is bound to the parent so the copied index validates.
        assert_eq!(s.uuid, p.uuid);
        assert_eq!(s.parent_uuid, p.uuid);
        assert_eq!(s.parent_volume, "vol1");
        // Size captured is the live size passed in, not the manifest's.
        assert_eq!(s.size_bytes, 2u64 << 30);
        // Family namespace resolves to the parent's effective namespace.
        assert_eq!(s.pool_namespace(), p.pool_namespace());
    }

    #[test]
    fn snapshot_of_a_clone_keeps_family_namespace() {
        // A clone carries dedup_namespace = family root. A snapshot of
        // that clone must key on the SAME family root, not the clone's
        // own uuid, or its hashes would bucket into the wrong pool.
        let family = [0x7Cu8; 16];
        let clone = parent("clone1", DedupScope::Local).with_dedup_namespace(family);
        let s = SnapshotManifest::new("s".into(), &clone, 1u64 << 30).unwrap();
        assert_eq!(s.uuid, clone.uuid); // header binds to the clone
        assert_eq!(s.dedup_namespace, family); // but pool keys on family
        assert_eq!(s.pool_namespace(), Some(namespace_from_uuid(&family)));
    }

    #[test]
    fn global_snapshot_has_no_namespace() {
        let p = parent("vol1", DedupScope::Global);
        let s = SnapshotManifest::new("snap1".into(), &p, 1u64 << 30).unwrap();
        assert_eq!(s.pool_namespace(), None);
    }

    #[test]
    fn persist_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let p =
            parent("vol1", DedupScope::Local).with_encryption(VolumeEncryptionAlgorithm::Aes256Gcm);
        let s = SnapshotManifest::new("snap1".into(), &p, 3u64 << 30).unwrap();
        let snap_dir = SnapshotManifest::dir_for(dir.path(), "vol1", "snap1");
        fs::create_dir_all(&snap_dir).unwrap();
        s.persist(&snap_dir).unwrap();

        let loaded = SnapshotManifest::load(dir.path(), "vol1", "snap1").unwrap();
        assert_eq!(loaded, s);
        assert!(loaded.encryption.is_some());
    }

    #[test]
    fn list_and_list_all_enumerate_snapshots() {
        let dir = TempDir::new().unwrap();
        // Two volumes; vol1 has two snapshots, vol2 has one.
        for (vol, snaps) in [("vol1", &["beta", "alpha"][..]), ("vol2", &["only"][..])] {
            let p = parent(vol, DedupScope::Local);
            p.clone().create(dir.path()).unwrap();
            for snap in snaps {
                let s = SnapshotManifest::new((*snap).into(), &p, 1u64 << 30).unwrap();
                let sd = SnapshotManifest::dir_for(dir.path(), vol, snap);
                fs::create_dir_all(&sd).unwrap();
                s.persist(&sd).unwrap();
            }
        }
        assert_eq!(
            SnapshotManifest::list(dir.path(), "vol1").unwrap(),
            vec!["alpha", "beta"]
        );
        let all = SnapshotManifest::list_all(dir.path()).unwrap();
        assert_eq!(
            all,
            vec![
                ("vol1".to_string(), "alpha".to_string()),
                ("vol1".to_string(), "beta".to_string()),
                ("vol2".to_string(), "only".to_string()),
            ]
        );
    }

    #[test]
    fn list_on_volume_without_snapshots_is_empty() {
        let dir = TempDir::new().unwrap();
        parent("vol1", DedupScope::Local)
            .create(dir.path())
            .unwrap();
        assert!(
            SnapshotManifest::list(dir.path(), "vol1")
                .unwrap()
                .is_empty()
        );
        assert!(SnapshotManifest::list_all(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn rejects_invalid_snapshot_name() {
        let p = parent("vol1", DedupScope::Local);
        for bad in ["", "with space", "weird/slash", ".."] {
            assert!(SnapshotManifest::new(bad.into(), &p, 1u64 << 30).is_err());
        }
    }
}
