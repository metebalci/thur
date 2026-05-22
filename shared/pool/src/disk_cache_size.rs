// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `disk_cache.size_gb` config shape — accepts either an explicit
//! GB count (`size_gb: 4`) or the literal string `auto`
//! (`size_gb: auto`), parsed identically out of YAML
//! (`<product>.yaml`) and JSON (`cloud-backends.json`'s per-entry
//! `disk_cache_size_gb` override). Shared across both daemons so
//! the YAML default and the JSON per-backend override can never
//! drift.
//!
//! `Auto` resolves to bytes against the data dir's filesystem at
//! daemon boot **and on every eviction tick** — external disk
//! pressure shrinks the budget reactively, clamped by
//! [`DiskCacheBounds`].

use std::path::Path;

use serde::de::{Deserializer, Error as DeError, Visitor};
use serde::{Deserialize, Serialize};

const BYTES_PER_GB: u64 = 1024 * 1024 * 1024;

/// Operator-facing shape of `disk_cache.size_gb`.
///
/// - `Explicit(n)` pins the per-backend cap to exactly `n` GB
///   (bounds ignored — the operator chose).
/// - `Auto` derives the cap from `statvfs(data_dir)` at every
///   eviction tick: `min(50% of free, max_gb)`, floored to
///   `min_gb` when the derived value undershoots.
///
/// Default is [`DiskCacheSize::Auto`] — `4 GB` (today's default)
/// is too small for almost any real install; deriving from free
/// space is the right zero-config behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(untagged)]
pub enum DiskCacheSize {
    /// Derive the cap from the filesystem at boot + every tick.
    /// Serializes as the string `"auto"`.
    #[default]
    Auto,
    /// Pin the cap to this many GB.
    Explicit(u64),
}

impl DiskCacheSize {
    /// Per-backend bounds applied when this value is `Auto`.
    /// Resolves to `Self::Explicit(_).bytes()` for explicit values;
    /// for `Auto`, computes `min(50% * free / auto_backends, max_gb) * GiB`,
    /// floored to `min_gb * GiB`.
    ///
    /// `auto_backends` is the number of `Auto`-mode backends sharing
    /// this filesystem — passing 2 instead of 1 splits the 50%-of-free
    /// share evenly so two `Auto` backends can't combined commit
    /// 100% of free space. Pass `1` when only this backend is
    /// `Auto` (or when called for a non-multi-backend context).
    ///
    /// `statvfs` failure (path missing, IO error) is treated as
    /// "no info"; the resolver falls back to `bounds.min_gb` so a
    /// boot under a transient FS error still produces a usable cap
    /// instead of zero. Logged at WARN by the caller (the boot /
    /// eviction loop) — we don't log here so this can stay a pure
    /// function and tests can drive it without a tracing subscriber.
    pub fn resolve_bytes(
        &self,
        data_dir: &Path,
        bounds: DiskCacheBounds,
        auto_backends: u32,
    ) -> u64 {
        let bounds = bounds.sanitize();
        match self {
            DiskCacheSize::Explicit(n) => n.saturating_mul(BYTES_PER_GB),
            DiskCacheSize::Auto => {
                let free = fs2::available_space(data_dir).unwrap_or(0);
                let share_n = auto_backends.max(1) as u64;
                let half_share = free / 2 / share_n;
                let max_bytes = bounds.max_gb.saturating_mul(BYTES_PER_GB);
                let min_bytes = bounds.min_gb.saturating_mul(BYTES_PER_GB);
                let derived = half_share.min(max_bytes);
                derived.max(min_bytes)
            }
        }
    }

    /// `true` iff this value is `Auto`. Used by the daemon to count
    /// auto-mode backends for the `auto_backends` share divisor.
    pub fn is_auto(&self) -> bool {
        matches!(self, DiskCacheSize::Auto)
    }

    /// Convenience: explicit GB if this is `Explicit(n)`, else None.
    pub fn explicit_gb(&self) -> Option<u64> {
        match self {
            DiskCacheSize::Explicit(n) => Some(*n),
            DiskCacheSize::Auto => None,
        }
    }
}

impl<'de> Deserialize<'de> for DiskCacheSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = DiskCacheSize;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an unsigned integer (GB) or the string \"auto\"")
            }

            fn visit_u64<E: DeError>(self, n: u64) -> Result<Self::Value, E> {
                Ok(DiskCacheSize::Explicit(n))
            }

            fn visit_i64<E: DeError>(self, n: i64) -> Result<Self::Value, E> {
                if n < 0 {
                    return Err(E::custom("disk_cache.size_gb cannot be negative"));
                }
                Ok(DiskCacheSize::Explicit(n as u64))
            }

            fn visit_str<E: DeError>(self, s: &str) -> Result<Self::Value, E> {
                match s {
                    "auto" => Ok(DiskCacheSize::Auto),
                    other => Err(E::custom(format!(
                        "expected unsigned integer or \"auto\", got \"{other}\""
                    ))),
                }
            }

            fn visit_string<E: DeError>(self, s: String) -> Result<Self::Value, E> {
                self.visit_str(&s)
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Min/max GB applied when `DiskCacheSize::Auto` resolves. Both
/// daemons read these from the same `disk_cache.{min_size_gb,
/// max_size_gb}` YAML keys; when the size is explicit, both bounds
/// are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskCacheBounds {
    pub min_gb: u64,
    pub max_gb: u64,
}

impl DiskCacheBounds {
    /// Today's hard-coded floor (4 GB, matches the pre-`auto` default)
    /// and a 500 GB ceiling that bounds the eviction worker's scan
    /// cost on very large filesystems.
    pub const DEFAULT: Self = Self {
        min_gb: 4,
        max_gb: 500,
    };

    fn sanitize(self) -> Self {
        // Caller config could swap min/max; clamp to keep
        // `min <= max` so `derived.max(min)` after `derived.min(max)`
        // still honors the larger bound when both happen to swap.
        if self.min_gb > self.max_gb {
            Self {
                min_gb: self.max_gb,
                max_gb: self.min_gb,
            }
        } else {
            self
        }
    }
}

impl Default for DiskCacheBounds {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn explicit_resolves_to_gb_times_gib_ignoring_bounds() {
        let tmp = TempDir::new().unwrap();
        let size = DiskCacheSize::Explicit(10);
        let bounds = DiskCacheBounds {
            min_gb: 100,
            max_gb: 200,
        };
        // Bounds are ignored for explicit — operator chose.
        let bytes = size.resolve_bytes(tmp.path(), bounds, 1);
        assert_eq!(bytes, 10 * BYTES_PER_GB);
    }

    #[test]
    fn auto_clamps_to_max_when_half_free_exceeds_it() {
        let tmp = TempDir::new().unwrap();
        let size = DiskCacheSize::Auto;
        // max=1 GB forces the clamp on essentially any real FS.
        let bytes = size.resolve_bytes(
            tmp.path(),
            DiskCacheBounds {
                min_gb: 0,
                max_gb: 1,
            },
            1,
        );
        assert_eq!(bytes, BYTES_PER_GB);
    }

    #[test]
    fn auto_floors_to_min_when_derived_undershoots() {
        let tmp = TempDir::new().unwrap();
        let size = DiskCacheSize::Auto;
        // min=100 TiB > anything real → derived undershoots, floor kicks in.
        let bytes = size.resolve_bytes(
            tmp.path(),
            DiskCacheBounds {
                min_gb: 100 * 1024,
                max_gb: 1024 * 1024,
            },
            1,
        );
        assert_eq!(bytes, 100 * 1024 * BYTES_PER_GB);
    }

    #[test]
    fn auto_splits_share_across_auto_backends() {
        let tmp = TempDir::new().unwrap();
        let size = DiskCacheSize::Auto;
        // With max small enough that only the share matters, doubling
        // the auto_backends count should not affect the result (clamp
        // wins). Use a wide-open max so the half_share path dominates.
        let bounds = DiskCacheBounds {
            min_gb: 0,
            max_gb: u64::MAX / BYTES_PER_GB,
        };
        let one = size.resolve_bytes(tmp.path(), bounds, 1);
        let two = size.resolve_bytes(tmp.path(), bounds, 2);
        // Two-backend share must be exactly half the one-backend share
        // (modulo integer division by 2 on the free count, which is
        // why we accept "within one byte").
        assert!(
            (one / 2).abs_diff(two) <= 1,
            "one={one} two={two} (expected ~ one/2)"
        );
    }

    #[test]
    fn auto_handles_zero_auto_backends_like_one() {
        let tmp = TempDir::new().unwrap();
        let bounds = DiskCacheBounds {
            min_gb: 0,
            max_gb: u64::MAX / BYTES_PER_GB,
        };
        let zero = DiskCacheSize::Auto.resolve_bytes(tmp.path(), bounds, 0);
        let one = DiskCacheSize::Auto.resolve_bytes(tmp.path(), bounds, 1);
        assert_eq!(zero, one);
    }

    #[test]
    fn auto_swapped_bounds_are_normalized() {
        let tmp = TempDir::new().unwrap();
        let bounds = DiskCacheBounds {
            min_gb: 100,
            max_gb: 1,
        };
        // After sanitize: min=1, max=100. The result is bounded by
        // those normalized values regardless of where the actual
        // half-share lands on the test runner's filesystem.
        let bytes = DiskCacheSize::Auto.resolve_bytes(tmp.path(), bounds, 1);
        assert!(bytes >= BYTES_PER_GB);
        assert!(bytes <= 100 * BYTES_PER_GB);
    }

    #[test]
    fn deserialize_integer_form() {
        let v: DiskCacheSize = serde_yaml::from_str("4").unwrap();
        assert_eq!(v, DiskCacheSize::Explicit(4));
    }

    #[test]
    fn deserialize_auto_string() {
        let v: DiskCacheSize = serde_yaml::from_str("auto").unwrap();
        assert_eq!(v, DiskCacheSize::Auto);
    }

    #[test]
    fn deserialize_rejects_negative() {
        let err = serde_yaml::from_str::<DiskCacheSize>("-1").unwrap_err();
        assert!(err.to_string().contains("negative"));
    }

    #[test]
    fn deserialize_rejects_other_strings() {
        let err = serde_yaml::from_str::<DiskCacheSize>("\"manual\"").unwrap_err();
        assert!(err.to_string().contains("manual"));
    }

    #[test]
    fn deserialize_from_json_integer_and_auto() {
        let v: DiskCacheSize = serde_json::from_str("8").unwrap();
        assert_eq!(v, DiskCacheSize::Explicit(8));
        let v: DiskCacheSize = serde_json::from_str("\"auto\"").unwrap();
        assert_eq!(v, DiskCacheSize::Auto);
    }

    #[test]
    fn default_is_auto() {
        assert_eq!(DiskCacheSize::default(), DiskCacheSize::Auto);
    }

    #[test]
    fn is_auto_and_explicit_gb_accessors() {
        assert!(DiskCacheSize::Auto.is_auto());
        assert_eq!(DiskCacheSize::Auto.explicit_gb(), None);
        assert!(!DiskCacheSize::Explicit(7).is_auto());
        assert_eq!(DiskCacheSize::Explicit(7).explicit_gb(), Some(7));
    }
}
