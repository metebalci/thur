// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! `memory_buffers.{write,read}_gb_per_tape` config shape — accepts
//! either an explicit GB count (`write_gb_per_tape: 10`) or the
//! literal string `auto` (`write_gb_per_tape: auto`).
//!
//! `Auto` resolves once at daemon boot against `/proc/meminfo
//! MemTotal` and the chassis `num_drives`. Total memory-buffer
//! footprint is bounded at boot to `auto_host_fraction_pct` of host
//! RAM, divided across drives, then split 2:1 between write and read
//! (preserving the historical `10 GB write` / `5 GB read` default
//! ratio). Each field is clamped to `[auto_min_gb_per_tape,
//! auto_max_gb_per_tape]`.
//!
//! Mirrors the `disk_cache_size` pattern in `shared-pool` — same
//! `Auto | Explicit(u64)` shape, same custom `Deserialize` accepting
//! integers or the literal `"auto"` string, same "fall back to
//! `bounds.min_gb` on probe failure" behavior. Lives in the VTL
//! daemon rather than `shared-pool` because today only the tape
//! product has an operator-configurable in-memory buffer surface;
//! when VSA's per-volume `PageCache` becomes operator-configurable
//! the type can move up.

use std::path::Path;

use serde::de::{Deserializer, Error as DeError, Visitor};
use serde::{Deserialize, Serialize};

const BYTES_PER_GB: u64 = 1024 * 1024 * 1024;

/// Numerator / denominator for splitting the per-tape auto budget
/// between write and read buffers under `Auto`. `2/3` for write,
/// `1/3` for read preserves the historical `10 GB / 5 GB` default.
pub const AUTO_WRITE_SHARE_NUM: u64 = 2;
pub const AUTO_WRITE_SHARE_DEN: u64 = 3;
pub const AUTO_READ_SHARE_NUM: u64 = 1;
pub const AUTO_READ_SHARE_DEN: u64 = 3;

/// Operator-facing shape of `memory_buffers.write_gb_per_tape` and
/// `memory_buffers.read_gb_per_tape`.
///
/// - `Explicit(n)` pins the per-tape buffer to exactly `n` GB
///   (bounds ignored — the operator chose). Total memory_buffers
///   footprint is still safety-checked at daemon start.
/// - `Auto` derives the per-tape buffer from `MemTotal` at boot.
///
/// Default is [`MemoryBuffersSize::Auto`] — fixed 10 GB / 5 GB
/// scales linearly with `num_drives` regardless of host RAM, and an
/// 8-drive default install on a 32 GB host would reserve 120 GB.
/// Auto-against-host is the right zero-config behavior; matches the
/// `disk_cache.size_gb` reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(untagged)]
pub enum MemoryBuffersSize {
    /// Derive the per-tape buffer from MemTotal at daemon boot.
    /// Serializes as the string `"auto"`.
    #[default]
    Auto,
    /// Pin this field to exactly this many GB.
    Explicit(u64),
}

impl MemoryBuffersSize {
    /// Bounds applied when this value is `Auto`. Explicit values
    /// ignore both bounds (operator chose).
    ///
    /// `host_mem_bytes` is `/proc/meminfo MemTotal` (use
    /// [`read_host_mem_bytes`] to probe). A read failure produces
    /// `0`, which sends auto resolution down the `min_gb` floor —
    /// the daemon still boots with a usable buffer instead of
    /// stalling, matching the disk_cache resolver's fallback.
    ///
    /// `num_drives` is the chassis drive count; `0` is treated as
    /// `1` so the divisor stays safe even if the library reports an
    /// empty drive list (degenerate test configs).
    ///
    /// `share_num / share_den` is this field's slice of the per-tape
    /// auto budget — use [`AUTO_WRITE_SHARE_NUM`] / [`AUTO_WRITE_SHARE_DEN`]
    /// for write, [`AUTO_READ_SHARE_NUM`] / [`AUTO_READ_SHARE_DEN`]
    /// for read.
    pub fn resolve_bytes(
        &self,
        host_mem_bytes: u64,
        num_drives: u32,
        auto_host_fraction_pct: u64,
        share_num: u64,
        share_den: u64,
        bounds: MemoryBuffersBounds,
    ) -> u64 {
        let bounds = bounds.sanitize();
        match self {
            MemoryBuffersSize::Explicit(n) => n.saturating_mul(BYTES_PER_GB),
            MemoryBuffersSize::Auto => {
                let drives = num_drives.max(1) as u64;
                let fraction = auto_host_fraction_pct.min(100);
                let total_budget = host_mem_bytes.saturating_mul(fraction) / 100;
                let per_tape = total_budget / drives;
                let share_den = share_den.max(1);
                let field_share = per_tape.saturating_mul(share_num) / share_den;
                let max_bytes = bounds.max_gb.saturating_mul(BYTES_PER_GB);
                let min_bytes = bounds.min_gb.saturating_mul(BYTES_PER_GB);
                let derived = field_share.min(max_bytes);
                derived.max(min_bytes)
            }
        }
    }

    /// `true` iff this value is `Auto`.
    pub fn is_auto(&self) -> bool {
        matches!(self, MemoryBuffersSize::Auto)
    }
}

impl<'de> Deserialize<'de> for MemoryBuffersSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = MemoryBuffersSize;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an unsigned integer (GB) or the string \"auto\"")
            }

            fn visit_u64<E: DeError>(self, n: u64) -> Result<Self::Value, E> {
                Ok(MemoryBuffersSize::Explicit(n))
            }

            fn visit_i64<E: DeError>(self, n: i64) -> Result<Self::Value, E> {
                if n < 0 {
                    return Err(E::custom("memory_buffers GB value cannot be negative"));
                }
                Ok(MemoryBuffersSize::Explicit(n as u64))
            }

            fn visit_str<E: DeError>(self, s: &str) -> Result<Self::Value, E> {
                match s {
                    "auto" => Ok(MemoryBuffersSize::Auto),
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

/// Min/max per-tape GB applied when [`MemoryBuffersSize::Auto`]
/// resolves. Both `write_gb_per_tape` and `read_gb_per_tape` share
/// the same bounds at the call site; explicit values ignore them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBuffersBounds {
    pub min_gb: u64,
    pub max_gb: u64,
}

impl MemoryBuffersBounds {
    /// Defaults: 1 GB floor (a single chunk plus some headroom) and
    /// 32 GB ceiling (caps the auto cap on hosts with hundreds of
    /// GB of RAM and few drives — that much per-tape RAM almost
    /// never improves throughput).
    pub const DEFAULT: Self = Self {
        min_gb: 1,
        max_gb: 32,
    };

    fn sanitize(self) -> Self {
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

impl Default for MemoryBuffersBounds {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Read `MemTotal` from `/proc/meminfo`, in bytes. Returns `0` on
/// any I/O or parse failure; callers should treat `0` as "no info"
/// — [`MemoryBuffersSize::resolve_bytes`] handles it by falling
/// back to the `min_gb` floor.
///
/// `/proc/meminfo`'s `MemTotal` line is reported in kibibytes
/// (`kB` label), e.g. `MemTotal:       32841104 kB`.
pub fn read_host_mem_bytes() -> u64 {
    read_host_mem_bytes_from(Path::new("/proc/meminfo"))
}

fn read_host_mem_bytes_from(path: &Path) -> u64 {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let trimmed = rest.trim();
            let num: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(kb) = num.parse::<u64>() {
                return kb.saturating_mul(1024);
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const _4_GIB: u64 = 4 * BYTES_PER_GB;
    const _32_GIB: u64 = 32 * BYTES_PER_GB;
    const _64_GIB: u64 = 64 * BYTES_PER_GB;

    fn write_bounds() -> MemoryBuffersBounds {
        MemoryBuffersBounds::DEFAULT
    }

    #[test]
    fn explicit_resolves_to_gb_times_gib_ignoring_bounds() {
        let bytes = MemoryBuffersSize::Explicit(64).resolve_bytes(
            _32_GIB,
            8,
            50,
            AUTO_WRITE_SHARE_NUM,
            AUTO_WRITE_SHARE_DEN,
            MemoryBuffersBounds {
                min_gb: 1,
                max_gb: 8,
            },
        );
        // Bounds ignored for explicit — operator chose 64 GB.
        assert_eq!(bytes, 64 * BYTES_PER_GB);
    }

    #[test]
    fn auto_scales_with_host_ram_and_drive_count() {
        // 64 GiB MemTotal, 2 drives, 50% fraction, 2/3 write share:
        //   total = 32 GiB; per_tape = 16 GiB; write share = 16 * 2/3 = ~10.67 GiB.
        let bytes = MemoryBuffersSize::Auto.resolve_bytes(
            _64_GIB,
            2,
            50,
            AUTO_WRITE_SHARE_NUM,
            AUTO_WRITE_SHARE_DEN,
            // Wide-open bounds so clamping doesn't dominate.
            MemoryBuffersBounds {
                min_gb: 0,
                max_gb: 10_000,
            },
        );
        let expected = (_64_GIB / 2) / 2 * 2 / 3;
        assert_eq!(bytes, expected);
    }

    #[test]
    fn auto_clamps_to_max_when_share_exceeds_it() {
        // Huge host (1 TiB RAM), 1 drive, default 32 GB ceiling — must clamp.
        let bytes = MemoryBuffersSize::Auto.resolve_bytes(
            1024 * BYTES_PER_GB,
            1,
            50,
            AUTO_WRITE_SHARE_NUM,
            AUTO_WRITE_SHARE_DEN,
            write_bounds(),
        );
        assert_eq!(bytes, 32 * BYTES_PER_GB);
    }

    #[test]
    fn auto_floors_to_min_when_share_undershoots() {
        // Tiny host (2 GiB) with many drives — share rounds toward 0,
        // floor must kick in.
        let bytes = MemoryBuffersSize::Auto.resolve_bytes(
            2 * BYTES_PER_GB,
            16,
            50,
            AUTO_WRITE_SHARE_NUM,
            AUTO_WRITE_SHARE_DEN,
            write_bounds(),
        );
        assert_eq!(bytes, BYTES_PER_GB);
    }

    #[test]
    fn auto_floors_to_min_when_host_mem_unknown() {
        // host_mem_bytes=0 simulates a /proc/meminfo read failure.
        let bytes = MemoryBuffersSize::Auto.resolve_bytes(
            0,
            8,
            50,
            AUTO_WRITE_SHARE_NUM,
            AUTO_WRITE_SHARE_DEN,
            write_bounds(),
        );
        assert_eq!(bytes, BYTES_PER_GB);
    }

    #[test]
    fn auto_zero_drives_does_not_divide_by_zero() {
        // Empty drive list (test config) — divisor must coerce to 1.
        let with_zero = MemoryBuffersSize::Auto.resolve_bytes(
            _64_GIB,
            0,
            50,
            AUTO_WRITE_SHARE_NUM,
            AUTO_WRITE_SHARE_DEN,
            MemoryBuffersBounds {
                min_gb: 0,
                max_gb: 10_000,
            },
        );
        let with_one = MemoryBuffersSize::Auto.resolve_bytes(
            _64_GIB,
            1,
            50,
            AUTO_WRITE_SHARE_NUM,
            AUTO_WRITE_SHARE_DEN,
            MemoryBuffersBounds {
                min_gb: 0,
                max_gb: 10_000,
            },
        );
        assert_eq!(with_zero, with_one);
    }

    #[test]
    fn auto_fraction_clamped_to_100_pct() {
        let bytes = MemoryBuffersSize::Auto.resolve_bytes(
            _64_GIB,
            1,
            200, // operator typoed 200%; we clamp to 100.
            1,
            1, // whole share, no split
            MemoryBuffersBounds {
                min_gb: 0,
                max_gb: 10_000,
            },
        );
        assert_eq!(bytes, _64_GIB);
    }

    #[test]
    fn auto_write_vs_read_split_preserves_2_to_1_ratio() {
        let write = MemoryBuffersSize::Auto.resolve_bytes(
            _64_GIB,
            1,
            50,
            AUTO_WRITE_SHARE_NUM,
            AUTO_WRITE_SHARE_DEN,
            MemoryBuffersBounds {
                min_gb: 0,
                max_gb: 10_000,
            },
        );
        let read = MemoryBuffersSize::Auto.resolve_bytes(
            _64_GIB,
            1,
            50,
            AUTO_READ_SHARE_NUM,
            AUTO_READ_SHARE_DEN,
            MemoryBuffersBounds {
                min_gb: 0,
                max_gb: 10_000,
            },
        );
        // Write is roughly twice read; allow one byte of integer-division drift.
        assert!(write.abs_diff(read.saturating_mul(2)) <= 1);
    }

    #[test]
    fn auto_swapped_bounds_are_normalized() {
        // min > max swap is sanitized so the result stays in
        // [min(max, min), max(max, min)].
        let bytes = MemoryBuffersSize::Auto.resolve_bytes(
            _64_GIB,
            1,
            50,
            AUTO_WRITE_SHARE_NUM,
            AUTO_WRITE_SHARE_DEN,
            MemoryBuffersBounds {
                min_gb: 100,
                max_gb: 1,
            },
        );
        assert!(bytes >= BYTES_PER_GB);
        assert!(bytes <= 100 * BYTES_PER_GB);
    }

    #[test]
    fn deserialize_integer_form() {
        let v: MemoryBuffersSize = serde_yaml::from_str("10").unwrap();
        assert_eq!(v, MemoryBuffersSize::Explicit(10));
    }

    #[test]
    fn deserialize_auto_string() {
        let v: MemoryBuffersSize = serde_yaml::from_str("auto").unwrap();
        assert_eq!(v, MemoryBuffersSize::Auto);
    }

    #[test]
    fn deserialize_rejects_negative() {
        let err = serde_yaml::from_str::<MemoryBuffersSize>("-3").unwrap_err();
        assert!(err.to_string().contains("negative"));
    }

    #[test]
    fn deserialize_rejects_other_strings() {
        let err = serde_yaml::from_str::<MemoryBuffersSize>("\"manual\"").unwrap_err();
        assert!(err.to_string().contains("manual"));
    }

    #[test]
    fn default_is_auto() {
        assert_eq!(MemoryBuffersSize::default(), MemoryBuffersSize::Auto);
    }

    #[test]
    fn is_auto_accessor() {
        assert!(MemoryBuffersSize::Auto.is_auto());
        assert!(!MemoryBuffersSize::Explicit(5).is_auto());
    }

    #[test]
    fn read_host_mem_parses_meminfo_format() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut f = tmp.reopen().unwrap();
        writeln!(f, "MemTotal:       32841104 kB").unwrap();
        writeln!(f, "MemFree:         1234567 kB").unwrap();
        f.sync_all().unwrap();

        let bytes = read_host_mem_bytes_from(tmp.path());
        assert_eq!(bytes, 32_841_104u64 * 1024);
    }

    #[test]
    fn read_host_mem_returns_zero_on_missing_file() {
        let bytes = read_host_mem_bytes_from(Path::new("/nonexistent/path/no-meminfo"));
        assert_eq!(bytes, 0);
    }

    #[test]
    fn read_host_mem_returns_zero_when_memtotal_line_absent() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut f = tmp.reopen().unwrap();
        writeln!(f, "MemFree:         1234567 kB").unwrap();
        f.sync_all().unwrap();

        let bytes = read_host_mem_bytes_from(tmp.path());
        assert_eq!(bytes, 0);
    }
}
