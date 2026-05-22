// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Per-crate `include_str!` anchors for `thurvtl config <X>`. The
//! actual `print!` happens in `shared_cli::emit_defaults` /
//! `emit_systemd_unit`; only the path-relative `include_str!` calls
//! must live here. The yaml reference is mirrored to
//! `dist/thurvtl.defaults.yaml` by `vtl/cli/build.rs`.

const REFERENCE_TEMPLATE: &str = include_str!("defaults_reference.yaml");
const SYSTEMD_UNIT_TEMPLATE: &str = include_str!("../../../../release/thurvtld.service");

pub fn reference_content() -> &'static str {
    REFERENCE_TEMPLATE
}

pub fn systemd_unit_content() -> &'static str {
    SYSTEMD_UNIT_TEMPLATE
}
