// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Dedup scope for content-addressed chunk pools — shared between
//! the tape (`core-stream`) and block (`core-block`) products so the
//! upload pipeline can carry one canonical enum across the boundary.
//!
//! Both products' manifest formats serialise this as the lowercase
//! variant name (`"local"` / `"global"`); the `#[serde(rename_all)]`
//! attribute below matches what each product's manifest already
//! emits.
//!
//! Lifted from `core_stream::cartridge::DedupScope` (tape side) and
//! `core_block::volume::DedupScope` (block side) — both product
//! crates now re-export from here.

use serde::{Deserialize, Serialize};

/// Scope of content-addressed dedup. Chunks are always
/// content-addressed (BLAKE3-keyed); this enum controls whether they
/// collapse cross-namespace (`Global` — one pool per backend) or
/// stay isolated per cartridge / volume (`Local`).
///
/// `Default` is [`DedupScope::Global`] so legacy tape manifests
/// without an explicit `dedup` field deserialise to the historical
/// VTL default (cross-cartridge dedup). VSA always sets the scope
/// explicitly at volume-create time and never relies on this
/// default, so the choice here only affects tape's legacy
/// `#[serde(default)]` deserialisers.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum DedupScope {
    /// Per-cartridge or per-volume namespace. Identical bytes from
    /// two cartridges (or volumes) seal to separate pool entries; no
    /// cross-namespace sharing. Use for tenant isolation or
    /// per-namespace cleanup.
    Local,
    /// Shared per-backend pool. Identical bytes from any pair of
    /// `Global` namespaces on the same backend collapse into one
    /// pool entry / one cloud object — cross-namespace dedup.
    #[default]
    Global,
}

impl DedupScope {
    /// Canonical lowercase form. Matches the serde tag for
    /// deserialisation parity.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Global => "global",
        }
    }

    /// `None` for `Global` (shared pool), `Some(label)` for `Local`
    /// (per-namespace). Matches the namespace argument on
    /// `ChunkPool::new_namespaced` and `ChunkPool::cloud_key_for`.
    pub fn namespace(self, label: &str) -> Option<&str> {
        match self {
            Self::Global => None,
            Self::Local => Some(label),
        }
    }
}

impl std::fmt::Display for DedupScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DedupScope {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "global" => Ok(Self::Global),
            other => Err(format!(
                "invalid dedup scope '{other}': expected 'local' or 'global'"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_lowercase() {
        let s = serde_json::to_string(&DedupScope::Local).unwrap();
        assert_eq!(s, "\"local\"");
        let back: DedupScope = serde_json::from_str("\"global\"").unwrap();
        assert_eq!(back, DedupScope::Global);
    }

    #[test]
    fn from_str_accepts_mixed_case_and_whitespace() {
        assert_eq!("Local".parse::<DedupScope>().unwrap(), DedupScope::Local);
        assert_eq!(
            " GLOBAL ".parse::<DedupScope>().unwrap(),
            DedupScope::Global
        );
        assert!("nope".parse::<DedupScope>().is_err());
    }

    #[test]
    fn namespace_resolves_per_variant() {
        assert_eq!(DedupScope::Global.namespace("vol1"), None);
        assert_eq!(DedupScope::Local.namespace("vol1"), Some("vol1"));
    }
}
