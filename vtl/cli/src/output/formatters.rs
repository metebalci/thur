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
