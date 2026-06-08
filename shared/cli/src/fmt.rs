// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Human-readable byte formatting for CLI output.
//!
//! Shared so `thurvtl cartridge info` / `library info` and
//! `thurvsa volume info` render byte counters identically. Binary
//! IEC suffixes (`KiB` / `MiB` / `TiB`) — the chunk-pool, page, and
//! capacity figures these print are all powers of two, so the IEC
//! units divide cleanly.

/// Friendly human-readable size formatter (binary IEC suffixes).
///
/// Sub-`KiB` values print as a bare byte count; larger values get
/// two decimals, dropping to zero decimals when the value is within
/// 5% of a whole unit (`1.00 GiB` reads better as `1 GiB`).
pub fn format_bytes(n: u64) -> String {
    const KIB: u64 = 1u64 << 10;
    const MIB: u64 = 1u64 << 20;
    const GIB: u64 = 1u64 << 30;
    const TIB: u64 = 1u64 << 40;
    const PIB: u64 = 1u64 << 50;

    let (val, unit) = if n >= PIB {
        (n as f64 / PIB as f64, "PiB")
    } else if n >= TIB {
        (n as f64 / TIB as f64, "TiB")
    } else if n >= GIB {
        (n as f64 / GIB as f64, "GiB")
    } else if n >= MIB {
        (n as f64 / MIB as f64, "MiB")
    } else if n >= KIB {
        (n as f64 / KIB as f64, "KiB")
    } else {
        return format!("{n} B");
    };
    if val.fract() < 0.05 {
        format!("{:.0} {}", val, unit)
    } else {
        format!("{:.2} {}", val, unit)
    }
}

/// Format a backend (storage) byte counter, appending its share of the
/// matching host counter — e.g. `2.31 TiB (50% of host)`. On the
/// write side that ratio is the dedup + compression saving; on the
/// read side it is the fraction of host bytes that missed cache. The
/// parenthetical is dropped when the host counter is 0 (a fresh
/// volume / cartridge, where the ratio would be undefined).
pub fn with_host_ratio(backend: u64, host: u64) -> String {
    let formatted = format_bytes(backend);
    if host == 0 {
        return formatted;
    }
    // Rounded integer percent; u128 so a PiB-scale counter can't
    // overflow the `* 100`.
    let pct = (backend as u128 * 100 + host as u128 / 2) / host as u128;
    format!("{formatted} ({pct}% of host)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1 KiB");
        assert_eq!(format_bytes(1usize as u64 * 1024 * 1024), "1 MiB");
        assert_eq!(format_bytes(1536 * 1024 * 1024), "1.50 GiB");
        assert_eq!(format_bytes(1u64 << 40), "1 TiB");
    }

    #[test]
    fn host_ratio_drops_parenthetical_when_host_zero() {
        assert_eq!(with_host_ratio(4096, 0), "4 KiB");
    }

    #[test]
    fn host_ratio_renders_rounded_percent() {
        assert_eq!(with_host_ratio(500, 1000), "500 B (50% of host)");
        // 2 of 3 rounds to 67%.
        assert_eq!(with_host_ratio(2, 3), "2 B (67% of host)");
    }
}
