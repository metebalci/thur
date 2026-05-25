// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cross-region DR restore driver — `thurvtl library restore`.
//!
//! Bring a fresh host up from a cold mirror bucket. The cloud bucket
//! already holds, per cartridge:
//!   manifests/<barcode>/manifest-latest.json   (sentinel)
//!   manifests/<barcode>/manifest-<ts>.json     (versioned)
//!   manifests/<barcode>/chunks/page-NNNNNN.dat
//!   manifests/<barcode>/blocks-pN/page-NNNNNN.dat
//!   chunks/.../<hash>.dat                       (content-addressed pool)
//!
//! This module wraps the single-cartridge cold-bucket primitive
//! ([`Cartridge::open_with_cloud_async`] in
//! `core/ssc/src/cartridge/mod.rs::load_manifest_async`) with a
//! discovery + batch driver. Chunks are NOT downloaded here; they
//! lazy-load on first host READ via the existing
//! `read_block_async` cloud-refetch path. Restore is metadata-only.

use crate::cartridge::{Cartridge, CartridgeOpenMode};
use crate::errors::Result;
use shared_object_store::ObjectStoreBackend;
use std::collections::BTreeSet;
use std::path::Path;

/// Outcome of a single cartridge restore attempt.
#[derive(Debug)]
pub struct CartridgeOutcome {
    pub barcode: String,
    /// Stringified error on failure (the underlying [`crate::errors::SmcError`]
    /// is not `Clone`); `Ok(())` on success.
    pub result: std::result::Result<(), String>,
}

/// Per-invocation report from [`run_restore`].
#[derive(Debug)]
pub struct RestoreReport {
    /// Backend name the operator targeted (forwarded for audit).
    pub backend_name: String,
    /// Every barcode whose `manifest-latest.json` sentinel was found
    /// under `manifests/` in the bucket.
    pub discovered: Vec<String>,
    /// Barcodes the operator's `--barcodes` filter excluded.
    pub filtered_out: Vec<String>,
    /// Barcodes skipped because the local cartridge directory already
    /// exists (`--allow-existing`).
    pub skipped_existing: Vec<String>,
    /// Per-cartridge restore outcomes (one entry per barcode actually
    /// attempted).
    pub cartridges: Vec<CartridgeOutcome>,
    /// True if the caller passed `dry_run`. Set so callers can suppress
    /// audit/inventory writes uniformly.
    pub dry_run: bool,
}

impl RestoreReport {
    pub fn successes(&self) -> Vec<&str> {
        self.cartridges
            .iter()
            .filter(|c| c.result.is_ok())
            .map(|c| c.barcode.as_str())
            .collect()
    }

    pub fn failures(&self) -> Vec<&str> {
        self.cartridges
            .iter()
            .filter(|c| c.result.is_err())
            .map(|c| c.barcode.as_str())
            .collect()
    }
}

/// Enumerate cartridges discoverable in a backend's `manifests/`
/// prefix. Only barcodes that have a `manifest-latest.json` sentinel
/// are returned — index-page-only entries (torn upload, in-progress
/// new cartridge) are skipped silently. Result is sorted
/// lexicographically.
pub async fn discover_cloud_cartridges(backend: &dyn ObjectStoreBackend) -> Result<Vec<String>> {
    let keys = backend.list_objects("manifests/").await?;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for key in keys {
        if let Some(barcode) = parse_sentinel_barcode(&key) {
            seen.insert(barcode);
        }
    }
    Ok(seen.into_iter().collect())
}

/// Extract `<barcode>` from `manifests/<barcode>/manifest-latest.json`;
/// returns `None` for any other key shape. A barcode with `/` is
/// rejected for safety even though the writer never emits one.
fn parse_sentinel_barcode(key: &str) -> Option<String> {
    let rest = key.strip_prefix("manifests/")?;
    let barcode = rest.strip_suffix("/manifest-latest.json")?;
    if barcode.is_empty() || barcode.contains('/') {
        return None;
    }
    Some(barcode.to_string())
}

/// Run a batch restore: discover → filter → per-cartridge restore.
///
/// `tapes_dir` is `<data_dir>/tapes/`. The caller (CLI) is responsible
/// for the inventory rebuild after this returns — see
/// `thurvtl library restore` for the wiring.
///
/// A per-cartridge failure does NOT abort the batch: the loop
/// continues, the failure is recorded, and the report carries every
/// outcome. Caller decides exit status from `report.failures()`.
pub async fn run_restore(
    tapes_dir: &Path,
    backend: &dyn ObjectStoreBackend,
    backend_name: &str,
    barcode_filter: &[String],
    allow_existing: bool,
    dry_run: bool,
) -> Result<RestoreReport> {
    let discovered = discover_cloud_cartridges(backend).await?;

    let filter_set: BTreeSet<&str> = barcode_filter.iter().map(|s| s.as_str()).collect();
    let (selected, filtered_out): (Vec<String>, Vec<String>) =
        discovered.iter().cloned().partition(|b| {
            if filter_set.is_empty() {
                true
            } else {
                filter_set.contains(b.as_str())
            }
        });

    let mut skipped_existing = Vec::new();
    let mut cartridges = Vec::new();

    if dry_run {
        // Don't touch the filesystem. The caller renders the
        // selected list as the dry-run output.
        return Ok(RestoreReport {
            backend_name: backend_name.to_string(),
            discovered,
            filtered_out,
            skipped_existing,
            cartridges,
            dry_run: true,
        });
    }

    for barcode in &selected {
        let cart_root = tapes_dir.join(barcode);
        if cart_root.exists() {
            if allow_existing {
                skipped_existing.push(barcode.clone());
                continue;
            }
            cartridges.push(CartridgeOutcome {
                barcode: barcode.clone(),
                result: Err(format!(
                    "local cartridge dir already exists at {}; \
                     pass --allow-existing to skip, or clean the data dir first",
                    cart_root.display()
                )),
            });
            continue;
        }
        let result = Cartridge::open_with_cloud_async(
            tapes_dir,
            barcode,
            CartridgeOpenMode::Open,
            Some(backend.clone_box()),
        )
        .await;
        let outcome = match result {
            Ok(_cart) => Ok(()),
            Err(e) => Err(e.to_string()),
        };
        cartridges.push(CartridgeOutcome {
            barcode: barcode.clone(),
            result: outcome,
        });
    }

    Ok(RestoreReport {
        backend_name: backend_name.to_string(),
        discovered,
        filtered_out,
        skipped_existing,
        cartridges,
        dry_run: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_object_store::LocalBackend;
    use std::fs;
    use tempfile::TempDir;

    /// Write a minimal `manifest-latest.json` for `barcode` under the
    /// LocalBackend's root. `index_epoch` is empty so the cartridge
    /// open path treats it as a legacy manifest (no per-partition
    /// blocks index needed for the test). Manifest carries the
    /// canonical fields `Cartridge::open` insists on.
    fn write_cloud_sentinel(backend_dir: &Path, barcode: &str) {
        let sentinel = backend_dir.join(format!("manifests/{barcode}/manifest-latest.json"));
        fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        let body = format!(
            r#"{{"label":"{barcode}","backend":"primary","dedup":"global",
                 "uuid":"{}","index_epoch":{{}}}}"#,
            "00".repeat(16)
        );
        fs::write(&sentinel, body).unwrap();
    }

    /// Write only an index-page object — no sentinel. Mimics a torn
    /// upload (page-uploads went through, sentinel-write crashed).
    fn write_cloud_orphan_page(backend_dir: &Path, barcode: &str) {
        let page = backend_dir.join(format!("manifests/{barcode}/chunks/page-000000.dat"));
        fs::create_dir_all(page.parent().unwrap()).unwrap();
        fs::write(&page, b"orphan-page").unwrap();
    }

    #[tokio::test]
    async fn discover_returns_sorted_sentinel_only() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path()).await.unwrap();
        write_cloud_sentinel(dir.path(), "TAPE002");
        write_cloud_sentinel(dir.path(), "TAPE001");
        write_cloud_orphan_page(dir.path(), "TAPE_TORN");
        let found = discover_cloud_cartridges(&backend).await.unwrap();
        assert_eq!(found, vec!["TAPE001".to_string(), "TAPE002".to_string()]);
    }

    #[tokio::test]
    async fn discover_empty_backend_returns_empty() {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path()).await.unwrap();
        let found = discover_cloud_cartridges(&backend).await.unwrap();
        assert!(found.is_empty(), "{:?}", found);
    }

    #[tokio::test]
    async fn run_restore_dry_run_writes_nothing() {
        let backend_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(backend_dir.path()).await.unwrap();
        write_cloud_sentinel(backend_dir.path(), "TAPE001");

        let data_dir = TempDir::new().unwrap();
        let tapes = data_dir.path().join("tapes");
        fs::create_dir_all(&tapes).unwrap();

        let report = run_restore(&tapes, &backend, "mirror", &[], false, true)
            .await
            .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.discovered, vec!["TAPE001".to_string()]);
        assert!(report.cartridges.is_empty(), "{:#?}", report.cartridges);
        assert!(!tapes.join("TAPE001").exists(), "dry-run must not write");
    }

    #[tokio::test]
    async fn run_restore_failure_isolated_per_cartridge() {
        // End-to-end Cartridge::open requires a fully-shaped manifest
        // (non-empty partitions, populated chunks.idx, …) that's the
        // integration test's job to set up. What this unit test
        // asserts is the batch driver's contract: both discovered
        // barcodes are attempted and both produce per-cartridge
        // outcomes — neither one short-circuits the loop. Both
        // happen to fail here because the minimal sentinel doesn't
        // pass finalize_open_from_manifest; that's fine, the property
        // we care about is failure-isolation.
        let backend_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(backend_dir.path()).await.unwrap();
        write_cloud_sentinel(backend_dir.path(), "TAPE_A");
        let bad = backend_dir
            .path()
            .join("manifests/TAPE_B/manifest-latest.json");
        fs::create_dir_all(bad.parent().unwrap()).unwrap();
        fs::write(&bad, b"{not-json").unwrap();

        let data_dir = TempDir::new().unwrap();
        let tapes = data_dir.path().join("tapes");
        fs::create_dir_all(&tapes).unwrap();

        let report = run_restore(&tapes, &backend, "mirror", &[], false, false)
            .await
            .unwrap();
        let attempted: Vec<&str> = report
            .cartridges
            .iter()
            .map(|c| c.barcode.as_str())
            .collect();
        assert_eq!(attempted, vec!["TAPE_A", "TAPE_B"]);
        // Each cartridge carries its own error — failure on one did
        // not pre-empt the attempt on the other.
        for outcome in &report.cartridges {
            assert!(outcome.result.is_err(), "{:#?}", outcome);
        }
        let a_err = report.cartridges[0].result.as_ref().err().unwrap();
        let b_err = report.cartridges[1].result.as_ref().err().unwrap();
        assert_ne!(a_err, b_err, "errors must be distinct per cartridge");
    }

    #[tokio::test]
    async fn run_restore_barcode_filter_attempts_only_selected() {
        let backend_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(backend_dir.path()).await.unwrap();
        write_cloud_sentinel(backend_dir.path(), "TAPE001");
        write_cloud_sentinel(backend_dir.path(), "TAPE002");
        write_cloud_sentinel(backend_dir.path(), "TAPE003");

        let data_dir = TempDir::new().unwrap();
        let tapes = data_dir.path().join("tapes");
        fs::create_dir_all(&tapes).unwrap();

        let only = vec!["TAPE002".to_string()];
        let report = run_restore(&tapes, &backend, "mirror", &only, false, false)
            .await
            .unwrap();
        // The discovery list is unaffected by the filter.
        assert_eq!(
            report.discovered,
            vec![
                "TAPE001".to_string(),
                "TAPE002".to_string(),
                "TAPE003".to_string(),
            ],
        );
        // Only the selected barcode shows up in `cartridges` —
        // filtered-out ones aren't even attempted.
        let attempted: Vec<&str> = report
            .cartridges
            .iter()
            .map(|c| c.barcode.as_str())
            .collect();
        assert_eq!(attempted, vec!["TAPE002"]);
        assert!(report.filtered_out.contains(&"TAPE001".to_string()));
        assert!(report.filtered_out.contains(&"TAPE003".to_string()));
    }

    #[tokio::test]
    async fn run_restore_existing_dir_skipped_or_failed() {
        let backend_dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(backend_dir.path()).await.unwrap();
        write_cloud_sentinel(backend_dir.path(), "TAPE001");

        let data_dir = TempDir::new().unwrap();
        let tapes = data_dir.path().join("tapes");
        fs::create_dir_all(tapes.join("TAPE001")).unwrap();

        // Without --allow-existing: refuse.
        let r1 = run_restore(&tapes, &backend, "mirror", &[], false, false)
            .await
            .unwrap();
        assert_eq!(r1.failures(), vec!["TAPE001"]);
        assert!(r1.skipped_existing.is_empty());

        // With --allow-existing: skip silently.
        let r2 = run_restore(&tapes, &backend, "mirror", &[], true, false)
            .await
            .unwrap();
        assert!(r2.failures().is_empty(), "{:#?}", r2.cartridges);
        assert_eq!(r2.skipped_existing, vec!["TAPE001".to_string()]);
    }

    #[test]
    fn parse_sentinel_barcode_rejects_other_keys() {
        assert_eq!(
            parse_sentinel_barcode("manifests/TAPE001/manifest-latest.json").as_deref(),
            Some("TAPE001")
        );
        assert_eq!(
            parse_sentinel_barcode("manifests/TAPE001/manifest-1234567890.json"),
            None
        );
        assert_eq!(
            parse_sentinel_barcode("manifests/TAPE001/chunks/page-000000.dat"),
            None
        );
        assert_eq!(parse_sentinel_barcode("chunks/aa/bb/cc.dat"), None);
        assert_eq!(
            parse_sentinel_barcode("manifests//manifest-latest.json"),
            None
        );
        assert_eq!(
            parse_sentinel_barcode("manifests/dir1/dir2/manifest-latest.json"),
            None
        );
    }
}
