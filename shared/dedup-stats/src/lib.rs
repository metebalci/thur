// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Cross-product dedup math for `system stats`.
//!
//! Both products answer the same question — "how much does dedup
//! save, and how is each entity's footprint split between chunks it
//! owns exclusively and chunks it shares?" — but enumerate entities
//! differently: VTL walks each cartridge's `chunks.idx`, VSA walks
//! each volume's `pages.idx`. This crate is the shared remainder: the
//! caller reduces every entity to an [`EntityScan`] and
//! [`compute_dedup`] does the hash-bucketing and the exclusive/shared
//! split.
//!
//! It is a plain data boundary — no trait, no I/O, no serde. The
//! product-specific `StatsReport` (which fields, which wire shape)
//! stays in each daemon.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};

/// One scanned entity — a VTL cartridge or a VSA volume — reduced to
/// the inputs the dedup math needs.
#[derive(Debug, Clone)]
pub struct EntityScan {
    /// Human label: cartridge barcode or volume name. Unique within
    /// one report; echoed back on the matching [`EntityContribution`].
    pub label: String,
    /// Storage backend name.
    pub backend: String,
    /// `Some(ns)` for a local-scope (per-entity) pool, `None` for the
    /// backend-global shared pool. The `(backend, namespace)` pair
    /// keys the dedup bucket — chunks only dedup within one bucket.
    pub namespace: Option<String>,
    /// Distinct chunk hashes this entity references, each mapped to
    /// its byte size. When two entities report different sizes for
    /// the same hash the larger wins (mirrors the on-disk index's
    /// max-size reconciliation).
    pub chunks: HashMap<String, u64>,
}

/// Per-entity dedup contribution. Returned in the same order as the
/// `compute_dedup` input slice, so callers can `zip` it back against
/// their own per-entity metadata.
#[derive(Debug, Clone)]
pub struct EntityContribution {
    /// Echoes [`EntityScan::label`].
    pub label: String,
    /// Sum of this entity's distinct chunk sizes.
    pub unique_bytes: u64,
    /// Bytes in chunks no other entity in the same `(backend,
    /// namespace)` bucket references.
    pub exclusive_bytes: u64,
    /// `unique_bytes - exclusive_bytes` — bytes in chunks shared with
    /// at least one sibling entity.
    pub shared_bytes: u64,
}

/// Per-backend dedup totals. Sorted by backend name.
#[derive(Debug, Clone)]
pub struct BackendDedup {
    pub backend: String,
    /// Sum of distinct chunk sizes across every namespace bucket of
    /// this backend — the deduplicated pool footprint.
    pub unique_pool_bytes: u64,
}

/// Bucket every chunk hash by `(backend, namespace)`, then compute the
/// per-entity exclusive/shared split and the per-backend unique pool
/// bytes.
///
/// `EntityContribution`s come back parallel to `scans` (input order);
/// `BackendDedup`s come back sorted by backend name.
pub fn compute_dedup(scans: &[EntityScan]) -> (Vec<EntityContribution>, Vec<BackendDedup>) {
    type BucketKey = (String, Option<String>);
    // hash -> (max observed size, owner count). Only the owner COUNT is
    // ever read (`owners == 1` => exclusive), and each EntityScan's
    // `chunks` is a HashMap so an entity contributes a given hash at most
    // once — a plain counter is an exact replacement for the previous
    // `HashSet<String>` of labels, which cost ~250+ B/chunk (≈15 GB of
    // `system stats` job RAM at the ~60 M-chunk documented scale) for a
    // value never inspected (issue #168).
    type Bucket = HashMap<String, (u64, u32)>;

    let mut buckets: HashMap<BucketKey, Bucket> = HashMap::new();
    for scan in scans {
        let bucket = buckets
            .entry((scan.backend.clone(), scan.namespace.clone()))
            .or_default();
        for (h, &sz) in &scan.chunks {
            let entry = bucket.entry(h.clone()).or_insert((sz, 0));
            entry.0 = entry.0.max(sz);
            entry.1 += 1;
        }
    }

    let contributions = scans
        .iter()
        .map(|scan| {
            let bucket = buckets
                .get(&(scan.backend.clone(), scan.namespace.clone()))
                .expect("scanned entity must own a bucket");
            let mut unique = 0u64;
            let mut exclusive = 0u64;
            for (h, &sz) in &scan.chunks {
                unique = unique.saturating_add(sz);
                let owners = bucket.get(h).map(|(_, count)| *count).unwrap_or(1);
                if owners == 1 {
                    exclusive = exclusive.saturating_add(sz);
                }
            }
            EntityContribution {
                label: scan.label.clone(),
                unique_bytes: unique,
                exclusive_bytes: exclusive,
                shared_bytes: unique.saturating_sub(exclusive),
            }
        })
        .collect();

    // Per-backend pool bytes: sum the distinct chunk sizes of every
    // one of the backend's namespace buckets.
    let mut backend_totals: BTreeMap<String, u64> = BTreeMap::new();
    for ((backend, _ns), bucket) in &buckets {
        let unique: u64 = bucket.values().map(|(sz, _)| *sz).sum::<u64>();
        let total = backend_totals.entry(backend.clone()).or_insert(0);
        *total = total.saturating_add(unique);
    }
    let backends = backend_totals
        .into_iter()
        .map(|(backend, unique_pool_bytes)| BackendDedup {
            backend,
            unique_pool_bytes,
        })
        .collect();

    (contributions, backends)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(label: &str, ns: Option<&str>, chunks: &[(&str, u64)]) -> EntityScan {
        EntityScan {
            label: label.to_string(),
            backend: "primary".to_string(),
            namespace: ns.map(str::to_string),
            chunks: chunks.iter().map(|(h, s)| ((*h).to_string(), *s)).collect(),
        }
    }

    #[test]
    fn global_chunks_split_into_exclusive_and_shared() {
        // Two cartridges in the shared pool sharing chunk "aa".
        let scans = vec![
            scan("T1", None, &[("aa", 1024), ("bb", 2048)]),
            scan("T2", None, &[("aa", 1024), ("cc", 4096)]),
        ];
        let (contribs, backends) = compute_dedup(&scans);

        assert_eq!(contribs[0].label, "T1");
        assert_eq!(contribs[0].unique_bytes, 3072);
        assert_eq!(contribs[0].exclusive_bytes, 2048);
        assert_eq!(contribs[0].shared_bytes, 1024);

        assert_eq!(contribs[1].label, "T2");
        assert_eq!(contribs[1].unique_bytes, 5120);
        assert_eq!(contribs[1].exclusive_bytes, 4096);
        assert_eq!(contribs[1].shared_bytes, 1024);

        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].backend, "primary");
        // aa + bb + cc = 1024 + 2048 + 4096
        assert_eq!(backends[0].unique_pool_bytes, 7168);
    }

    #[test]
    fn local_namespaces_do_not_dedup_against_each_other() {
        // Same hash, different namespaces — each is exclusive to its
        // own entity and each bucket contributes its full size.
        let scans = vec![
            scan("V1", Some("ns1"), &[("aa", 1000)]),
            scan("V2", Some("ns2"), &[("aa", 1000)]),
        ];
        let (contribs, backends) = compute_dedup(&scans);

        assert_eq!(contribs[0].exclusive_bytes, 1000);
        assert_eq!(contribs[0].shared_bytes, 0);
        assert_eq!(contribs[1].exclusive_bytes, 1000);
        assert_eq!(contribs[1].shared_bytes, 0);
        // Two separate buckets, 1000 each.
        assert_eq!(backends[0].unique_pool_bytes, 2000);
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let (contribs, backends) = compute_dedup(&[]);
        assert!(contribs.is_empty());
        assert!(backends.is_empty());
    }
}
