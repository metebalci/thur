// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-volume runtime state — `<data_dir>/volumes/<name>/runtime.json`.
//!
//! Sidecar to `manifest.json`. Holds everything the daemon mutates
//! while running (host/backend byte counters, last-write timestamp);
//! the manifest
//! itself is creation-frozen so any rewrites on the hot path are
//! confined to this file. Splitting these out also lets
//! `volume key migrate` rewrite identity (`encryption.*`) without
//! racing the live `VolumeWriter`'s flush path.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::volume::{SyncAfter, VolumeError};

/// On-disk volume runtime state. Persisted alongside `manifest.json`
/// in the volume directory; rewritten at flush boundaries by
/// [`crate::cache::PageCache`] and by
/// `thurvsa volume modify --sync-after` flips.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VolumeRuntime {
    /// Lifetime host bytes written into this volume — pre-dedup,
    /// pre-compression. Bumped on every WRITE / committed CAW /
    /// UNMAP; reset to 0 at create.
    pub host_bytes_written: u64,
    /// Bytes served to the host for READs — logical, pre-dedup. Counts
    /// reads satisfied from cache and from cloud alike.
    /// `#[serde(default)]` so a pre-counter `runtime.json` (which had
    /// only `host_bytes_written`) deserialises this to 0.
    #[serde(default)]
    pub host_bytes_read: u64,
    /// Bytes actually PUT to cloud for this volume — post-dedup,
    /// post-compression, i.e. the real backend storage cost.
    /// `#[serde(default)]` for the same legacy-file reason.
    #[serde(default)]
    pub backend_bytes_written: u64,
    /// Bytes fetched from cloud on a page cache miss. Decompressed
    /// page bytes — equal to the on-wire size while chunks are stored
    /// uncompressed (VSA does not currently compress volume chunks).
    /// `#[serde(default)]` for the same legacy-file reason.
    #[serde(default)]
    pub backend_bytes_read: u64,
    /// Last persist timestamp. Advanced by the cache's flush path so
    /// an operator inspecting the file can tell when the daemon last
    /// committed runtime state.
    pub modified_at: DateTime<Utc>,
    /// Mutable durability tier for SCSI SYNCHRONIZE CACHE.
    /// `#[serde(default)]` so legacy runtime.json files without
    /// this field deserialise to the safe default
    /// ([`SyncAfter::Storage`]).
    #[serde(default)]
    pub sync_after: SyncAfter,
}

impl VolumeRuntime {
    pub const FILENAME: &'static str = "runtime.json";

    pub fn path_for(vol_dir: &Path) -> PathBuf {
        vol_dir.join(Self::FILENAME)
    }

    /// Build a freshly-zeroed runtime — used at `volume create` time.
    /// `sync_after` defaults to [`SyncAfter::Storage`]; the CLI's
    /// `--sync-after <MODE>` flag can override via
    /// [`Self::new_zero_with_sync_after`].
    pub fn new_zero() -> Self {
        Self::new_zero_with_sync_after(SyncAfter::default())
    }

    /// Build a freshly-zeroed runtime with an operator-supplied
    /// initial `sync_after`. Called by the admin `volume create`
    /// handler when the CLI passes `--sync-after`.
    pub fn new_zero_with_sync_after(sync_after: SyncAfter) -> Self {
        Self {
            host_bytes_written: 0,
            host_bytes_read: 0,
            backend_bytes_written: 0,
            backend_bytes_read: 0,
            modified_at: Utc::now(),
            sync_after,
        }
    }

    /// Atomic write: tmp + fsync + rename, matching `VolumeManifest::persist`.
    pub fn persist(&self, vol_dir: &Path) -> Result<(), VolumeError> {
        let tmp = vol_dir.join("runtime.json.tmp");
        let final_path = vol_dir.join(Self::FILENAME);
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

    /// Load runtime state from `<vol_dir>/runtime.json`. Returns
    /// `VolumeError::RuntimeMissing` if the file is absent — the
    /// volume's `manifest.json` exists in that case, which means
    /// either an interrupted create or hand-rolled corruption; the
    /// daemon refuses both rather than silently zero-initializing.
    pub fn load(vol_dir: &Path) -> Result<Self, VolumeError> {
        let path = Self::path_for(vol_dir);
        if !path.exists() {
            return Err(VolumeError::RuntimeMissing(path));
        }
        let raw = fs::read_to_string(&path)?;
        let parsed: Self = serde_json::from_str(&raw)?;
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trip_through_disk() {
        let dir = TempDir::new().unwrap();
        let r = VolumeRuntime {
            host_bytes_written: 123_456,
            host_bytes_read: 789_012,
            backend_bytes_written: 64_000,
            backend_bytes_read: 32_000,
            modified_at: Utc::now(),
            sync_after: SyncAfter::Disk,
        };
        r.persist(dir.path()).unwrap();
        let loaded = VolumeRuntime::load(dir.path()).unwrap();
        assert_eq!(loaded.host_bytes_written, 123_456);
        assert_eq!(loaded.host_bytes_read, 789_012);
        assert_eq!(loaded.backend_bytes_written, 64_000);
        assert_eq!(loaded.backend_bytes_read, 32_000);
        assert_eq!(loaded.sync_after, SyncAfter::Disk);
    }

    #[test]
    fn missing_file_returns_runtime_missing() {
        let dir = TempDir::new().unwrap();
        let err = VolumeRuntime::load(dir.path()).unwrap_err();
        assert!(matches!(err, VolumeError::RuntimeMissing(_)));
    }

    #[test]
    fn new_zero_starts_at_zero() {
        let r = VolumeRuntime::new_zero();
        assert_eq!(r.host_bytes_written, 0);
        assert_eq!(r.host_bytes_read, 0);
        assert_eq!(r.backend_bytes_written, 0);
        assert_eq!(r.backend_bytes_read, 0);
        // Default `sync_after` is the safest tier — explicit so a
        // future change to the enum default doesn't silently flip
        // newly-created volumes off cloud-durable.
        assert_eq!(r.sync_after, SyncAfter::Storage);
    }

    #[test]
    fn legacy_runtime_json_without_sync_after_defaults_to_cloud() {
        // Pre-knob runtime.json had only host_bytes_written +
        // modified_at. Deserialise with `#[serde(default)]` and
        // make sure the missing fields land on safe defaults — Cloud
        // for sync_after, 0 for the three later counter fields.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("runtime.json");
        std::fs::write(
            &path,
            r#"{"host_bytes_written":42,"modified_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let loaded = VolumeRuntime::load(dir.path()).unwrap();
        assert_eq!(loaded.host_bytes_written, 42);
        assert_eq!(loaded.sync_after, SyncAfter::Storage);
        assert_eq!(loaded.host_bytes_read, 0);
        assert_eq!(loaded.backend_bytes_written, 0);
        assert_eq!(loaded.backend_bytes_read, 0);
    }
}
