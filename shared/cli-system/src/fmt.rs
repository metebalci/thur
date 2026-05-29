// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Byte / ratio formatting for the `system stats` human output, shared
//! by both products' `stats` renderers.
//!
//! Distinct from [`shared_cli::format_bytes`] (used by `system
//! monitor`): the stats table always shows two decimals
//! (`2.00 KiB`) for column alignment, whereas the monitor formatter
//! trims trailing zeros (`2 KiB`). Keeping them separate preserves
//! each command's established output.

/// Format a byte count with always-two-decimal IEC units (B / KiB /
/// MiB / GiB / TiB) for the stats table.
pub fn fmt_bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    const TIB: f64 = GIB * 1024.0;
    let f = n as f64;
    if f >= TIB {
        format!("{:.2} TiB", f / TIB)
    } else if f >= GIB {
        format!("{:.2} GiB", f / GIB)
    } else if f >= MIB {
        format!("{:.2} MiB", f / MIB)
    } else if f >= KIB {
        format!("{:.2} KiB", f / KIB)
    } else {
        format!("{} B", n)
    }
}

/// Format a dedup ratio (`logical / unique`) as `N.NNx`, or an em dash
/// when `unique` is zero.
pub fn fmt_ratio(logical: u64, unique: u64) -> String {
    if unique == 0 {
        "—".into()
    } else {
        format!("{:.2}x", logical as f64 / unique as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_bytes_uses_two_decimals_per_unit() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2048), "2.00 KiB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.00 MiB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.00 GiB");
        assert_eq!(fmt_bytes(2 * 1024_u64.pow(4)), "2.00 TiB");
    }

    #[test]
    fn fmt_ratio_handles_zero_unique() {
        assert_eq!(fmt_ratio(100, 0), "—");
        assert_eq!(fmt_ratio(400, 100), "4.00x");
    }
}
