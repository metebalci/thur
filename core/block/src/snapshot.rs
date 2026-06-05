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
///
/// History:
/// - **v1** (issue #13) — initial frozen-page-table snapshot.
/// - **v2** (issue #86) — adds optional `crypto_uuid: [u8; 16]`, the
///   parent's crypto identity, copied through so a clone made from a
///   snapshot of an encrypted *clone* inherits the right IV/DEK
///   identity. `None` (the default for a snapshot of an origin volume,
///   whose `uuid` already is the crypto identity) keeps the file
///   byte-identical to v1 — no migration.
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 2;

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

/// `Option`-shaped variant of [`uuid_hex`] for the optional
/// `crypto_uuid`, so a snapshot of an origin volume omits the field.
mod opt_uuid_hex {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(uuid: &Option<[u8; 16]>, s: S) -> Result<S::Ok, S::Error> {
        match uuid {
            Some(u) => s.serialize_str(&hex::encode(u)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<[u8; 16]>, D::Error> {
        let opt = Option::<String>::deserialize(d)?;
        let Some(s) = opt else {
            return Ok(None);
        };
        let bytes = hex::decode(&s).map_err(D::Error::custom)?;
        if bytes.len() != 16 {
            return Err(D::Error::custom("uuid must be 16 bytes"));
        }
        let mut out = [0u8; 16];
        out.copy_from_slice(&bytes);
        Ok(Some(out))
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
    /// itself never decrypts (it is not host-visible); this records the
    /// keystore backend + wrapped DEK so a clone made from this snapshot
    /// can unwrap the *shared* DEK (issue #86).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<VolumeEncryptionMeta>,
    /// Crypto identity copied from the parent (issue #86), the value a
    /// clone made from this snapshot inherits as its own `crypto_uuid`.
    /// `None` for a snapshot of an *origin* volume (its `uuid` already
    /// is the crypto identity, and `uuid == parent_uuid` here);
    /// `Some(C)` for a snapshot of an encrypted *clone*, whose `uuid`
    /// differs from its crypto root `C`. Routed through
    /// [`Self::dek_uuid`].
    #[serde(
        default,
        with = "opt_uuid_hex",
        skip_serializing_if = "Option::is_none"
    )]
    pub crypto_uuid: Option<[u8; 16]>,
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
            crypto_uuid: parent.crypto_uuid,
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

    /// The crypto identity a clone made from this snapshot inherits: the
    /// copied-through `crypto_uuid` if set (snapshot of an encrypted
    /// clone), else this snapshot's `uuid` (snapshot of an origin
    /// volume, where `uuid` is the crypto root). Mirrors
    /// [`VolumeManifest::dek_uuid`] (issue #86).
    pub fn dek_uuid(&self) -> [u8; 16] {
        self.crypto_uuid.unwrap_or(self.uuid)
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

/// Refcount-by-scan over crypto identities (issue #86). Returns `true`
/// if any volume or snapshot manifest under `data_dir` keys its crypto
/// identity ([`VolumeManifest::dek_uuid`] / [`SnapshotManifest::dek_uuid`])
/// on `target` — other than the volume named `exclude` and its own
/// snapshots, when given.
///
/// This is the reference notion behind refcounted DEK custody: a DEK
/// shared by a source + clone family must not be `keystore.forget`-ten
/// (on `volume destroy`) or rewrapped (on `volume key migrate`) while
/// any other family member still needs it. There is no persistent
/// refcount — a chunk-pool-GC-style manifest walk is the source of
/// truth, so it can never drift from reality.
///
/// `exclude = None` is the destroy path: call it *after* removing the
/// volume's on-disk subtree, so the tree it walks is exactly the
/// survivors. `exclude = Some(name)` is the migrate path: the volume is
/// still present and must not count itself or its snapshots.
pub fn crypto_identity_referenced(
    data_dir: &Path,
    target: [u8; 16],
    exclude: Option<&str>,
) -> Result<bool, VolumeError> {
    for vol in VolumeManifest::list(data_dir)? {
        if exclude == Some(vol.as_str()) {
            // Skips this volume's manifest *and* its snapshots (walked
            // inside this loop body), i.e. the whole excluded subtree.
            continue;
        }
        if VolumeManifest::load(data_dir, &vol)?.dek_uuid() == target {
            return Ok(true);
        }
        for snap in SnapshotManifest::list(data_dir, &vol)? {
            if SnapshotManifest::load(data_dir, &vol, &snap)?.dek_uuid() == target {
                return Ok(true);
            }
        }
    }
    Ok(false)
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
    fn snapshot_propagates_crypto_identity() {
        // Origin volume: crypto_uuid None, so the snapshot's dek_uuid is
        // the parent uuid (== the snapshot's own uuid).
        let p = parent("vol1", DedupScope::Local);
        let s = SnapshotManifest::new("snap1".into(), &p, 1u64 << 30).unwrap();
        assert!(s.crypto_uuid.is_none());
        assert_eq!(s.dek_uuid(), p.uuid);

        // Snapshot of an encrypted clone: crypto_uuid copies through, so
        // dek_uuid stays the *source* identity even though the snapshot's
        // own uuid is the clone's (issue #86).
        let source = [0xC0u8; 16];
        let clone = parent("clone1", DedupScope::Local).with_crypto_uuid(source);
        let s2 = SnapshotManifest::new("s".into(), &clone, 1u64 << 30).unwrap();
        assert_eq!(s2.uuid, clone.uuid);
        assert_eq!(s2.crypto_uuid, Some(source));
        assert_eq!(s2.dek_uuid(), source);
    }

    #[test]
    fn crypto_identity_referenced_tracks_family() {
        let dir = TempDir::new().unwrap();
        let source = [0xC0u8; 16];

        // A clone keyed on the shared crypto identity `source`, plus an
        // unrelated volume on its own identity.
        let clone = parent("clone1", DedupScope::Local).with_crypto_uuid(source);
        clone.clone().create(dir.path()).unwrap();
        let other = parent("other", DedupScope::Local);
        other.clone().create(dir.path()).unwrap();

        // `source` is referenced by clone1, but not once clone1 is
        // excluded (the destroy-of-last-member case).
        assert!(crypto_identity_referenced(dir.path(), source, None).unwrap());
        assert!(!crypto_identity_referenced(dir.path(), source, Some("clone1")).unwrap());
        // `other`'s own identity is referenced by itself only.
        assert!(crypto_identity_referenced(dir.path(), other.uuid, None).unwrap());
        assert!(!crypto_identity_referenced(dir.path(), other.uuid, Some("other")).unwrap());
        // An unknown identity is referenced by nobody.
        assert!(!crypto_identity_referenced(dir.path(), [0x99u8; 16], None).unwrap());

        // A snapshot of clone1 also pins `source` — but excluding clone1
        // excludes its whole subtree (its own snapshots go with it on
        // destroy), so `source` is unreferenced under that exclusion.
        let snap = SnapshotManifest::new("snap".into(), &clone, 1u64 << 30).unwrap();
        let sd = SnapshotManifest::dir_for(dir.path(), "clone1", "snap");
        fs::create_dir_all(&sd).unwrap();
        snap.persist(&sd).unwrap();
        assert!(crypto_identity_referenced(dir.path(), source, None).unwrap());
        assert!(!crypto_identity_referenced(dir.path(), source, Some("clone1")).unwrap());

        // A *sibling* volume that also references `source` keeps it alive
        // even when clone1 is excluded — destroying clone1 must not
        // forget the shared DEK.
        parent("sibling", DedupScope::Local)
            .with_crypto_uuid(source)
            .create(dir.path())
            .unwrap();
        assert!(crypto_identity_referenced(dir.path(), source, Some("clone1")).unwrap());
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

    /// Write a snapshot manifest JSON `value` to the canonical path for
    /// `(parent, snap)` under `dir`, creating directories. Used by the
    /// on-disk compat tests that hand-tweak the serialized form.
    fn write_snap_json(dir: &Path, parent_vol: &str, snap: &str, value: &serde_json::Value) {
        let sd = SnapshotManifest::dir_for(dir, parent_vol, snap);
        fs::create_dir_all(&sd).unwrap();
        fs::write(sd.join(SnapshotManifest::FILENAME), value.to_string()).unwrap();
    }

    /// A v1 snapshot (issue #13: `schema_version` of 1, no `crypto_uuid`
    /// field) must still load: the version stamps forward to current,
    /// `crypto_uuid` reads as `None`, and `dek_uuid()` falls back to the
    /// snapshot's own uuid — so existing snapshots keep resolving after
    /// the issue #86 schema bump.
    #[test]
    fn v1_snapshot_loads_with_crypto_uuid_none() {
        let dir = TempDir::new().unwrap();
        let p = parent("vol1", DedupScope::Local);
        let s = SnapshotManifest::new("snap1".into(), &p, 1u64 << 30).unwrap();
        // An origin snapshot already omits crypto_uuid; downgrade the
        // version marker to reproduce the v1 on-disk shape exactly.
        let mut v = serde_json::to_value(&s).unwrap();
        assert!(
            v.get("crypto_uuid").is_none(),
            "origin snapshot omits crypto_uuid"
        );
        v["schema_version"] = serde_json::json!(1);
        write_snap_json(dir.path(), "vol1", "snap1", &v);

        let loaded = SnapshotManifest::load(dir.path(), "vol1", "snap1").unwrap();
        assert_eq!(loaded.schema_version, SNAPSHOT_SCHEMA_VERSION);
        assert!(loaded.crypto_uuid.is_none());
        assert_eq!(loaded.dek_uuid(), loaded.uuid);
    }

    /// `load` rejects an unrecognized schema version in either direction
    /// (a corrupt 0, or a future version this binary can't understand)
    /// rather than silently misreading the file.
    #[test]
    fn load_rejects_out_of_range_schema_version() {
        let dir = TempDir::new().unwrap();
        let p = parent("vol1", DedupScope::Local);
        let s = SnapshotManifest::new("snap1".into(), &p, 1u64 << 30).unwrap();
        let base = serde_json::to_value(&s).unwrap();

        for bad in [0u32, SNAPSHOT_SCHEMA_VERSION + 1, 99] {
            let mut v = base.clone();
            v["schema_version"] = serde_json::json!(bad);
            write_snap_json(dir.path(), "vol1", "snap1", &v);
            assert!(
                matches!(
                    SnapshotManifest::load(dir.path(), "vol1", "snap1"),
                    Err(VolumeError::SchemaMismatch { .. })
                ),
                "schema_version {bad} must be rejected"
            );
        }
    }

    /// A malformed uuid hex string is refused by the serde adapter — a
    /// truncated or non-hex `uuid` can't be coerced into a 16-byte array.
    #[test]
    fn load_rejects_malformed_uuid_hex() {
        let dir = TempDir::new().unwrap();
        let p = parent("vol1", DedupScope::Local);
        let s = SnapshotManifest::new("snap1".into(), &p, 1u64 << 30).unwrap();
        let mut v = serde_json::to_value(&s).unwrap();
        v["uuid"] = serde_json::json!("not-hex-zz");
        write_snap_json(dir.path(), "vol1", "snap1", &v);
        assert!(SnapshotManifest::load(dir.path(), "vol1", "snap1").is_err());
    }

    /// The optional `crypto_uuid` is omitted from an origin snapshot's
    /// JSON (`skip_serializing_if`) and present for an encrypted clone's
    /// — the `opt_uuid_hex` Some path round-trips.
    #[test]
    fn crypto_uuid_omitted_for_origin_present_for_clone() {
        let dir = TempDir::new().unwrap();
        // Origin: crypto_uuid None -> field absent in the file.
        let origin = parent("vol1", DedupScope::Local);
        let s = SnapshotManifest::new("snap1".into(), &origin, 1u64 << 30).unwrap();
        let sd = SnapshotManifest::dir_for(dir.path(), "vol1", "snap1");
        fs::create_dir_all(&sd).unwrap();
        s.persist(&sd).unwrap();
        let raw = fs::read_to_string(sd.join(SnapshotManifest::FILENAME)).unwrap();
        assert!(
            !raw.contains("crypto_uuid"),
            "origin snapshot must omit crypto_uuid"
        );

        // Encrypted clone: crypto_uuid Some(source) round-trips through
        // persist + load, and dek_uuid resolves to the shared identity.
        let source = [0xC0u8; 16];
        let clone = parent("clone1", DedupScope::Local)
            .with_crypto_uuid(source)
            .with_encryption(VolumeEncryptionAlgorithm::Aes256Gcm);
        let cs = SnapshotManifest::new("s".into(), &clone, 1u64 << 30).unwrap();
        let csd = SnapshotManifest::dir_for(dir.path(), "clone1", "s");
        fs::create_dir_all(&csd).unwrap();
        cs.persist(&csd).unwrap();
        let craw = fs::read_to_string(csd.join(SnapshotManifest::FILENAME)).unwrap();
        assert!(
            craw.contains("crypto_uuid"),
            "clone snapshot must record crypto_uuid"
        );

        let loaded = SnapshotManifest::load(dir.path(), "clone1", "s").unwrap();
        assert_eq!(loaded.crypto_uuid, Some(source));
        assert!(loaded.encryption.is_some());
        assert_eq!(loaded.dek_uuid(), source);
    }
}
