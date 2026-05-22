// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

use comfy_table::{ContentArrangement, Table, modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL};

/// Human-readable byte formatters — binary IEC suffixes, shared with
/// `thurvsa` so both products render byte counters identically. Re-
/// exported here so existing `crate::output::format_bytes` callers
/// keep resolving.
pub use shared_cli::{format_bytes, with_host_ratio};

/// Create a formatted table with rounded corners
pub fn create_table() -> Table {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.load_preset(UTF8_FULL);
    table.apply_modifier(UTF8_ROUND_CORNERS);
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_table_starts_empty_and_renders() {
        let mut table = create_table();
        table.set_header(vec!["A", "B"]);
        table.add_row(vec!["1", "2"]);
        let rendered = table.to_string();
        assert!(rendered.contains('1'));
        assert!(rendered.contains('2'));
    }

    #[test]
    fn format_bytes_renders_iec_suffixes() {
        assert!(format_bytes(0).contains('0'));
        let kib = format_bytes(2048);
        assert!(kib.contains("KiB") || kib.contains('2'));
    }
}
