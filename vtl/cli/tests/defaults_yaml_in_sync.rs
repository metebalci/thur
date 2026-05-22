// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Guard rail: keep the checked-in `dist/thurvtl.defaults.yaml` in
//! lockstep with the in-code defaults. Any time a config key is
//! added, removed, or its default changes, the generator output drifts
//! from the file on disk and this test fails with a hint to regenerate.

use std::fs;
use std::path::PathBuf;

const DEFAULTS_PATH: &str = "dist/thurvtl.defaults.yaml";

/// Repo root = workspace root. Layout C puts this crate at
/// `<workspace>/vtl/cli/`, so the workspace root is two parents
/// up from `CARGO_MANIFEST_DIR`.
fn repo_root() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .expect("thurvtl must live two levels under the repo root")
        .to_path_buf()
}

#[path = "../src/commands/generate_config.rs"]
#[allow(dead_code)]
mod generate_config;

#[test]
fn defaults_yaml_matches_generator_output() {
    let path = repo_root().join(DEFAULTS_PATH);
    let on_disk =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));

    // The generator prints with a trailing newline (println!), and the
    // file on disk should be the captured stdout. Match by appending the
    // same newline the generator emits.
    let generated = generate_config::reference_content().to_string();

    if on_disk != generated {
        panic!(
            "{} is out of sync with the in-code defaults reference.\n\
             Regenerate it with:\n\
             \n    cargo run -p thurvtl -- generate-config --defaults > {}\n",
            DEFAULTS_PATH, DEFAULTS_PATH
        );
    }
}
