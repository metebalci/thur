// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Sync guard for `docs/reference/openapi.yaml` (issue #12).
//!
//! Parses thurvtld's TCP HTTP router source (`src/http/mod.rs`) for the
//! routes it `.route(...)`-mounts and fails if any of them is missing
//! from the hand-written spec's `paths`. This is what keeps the spec
//! "in sync with the code, not the docs" without a code-gen pipeline:
//! add a TCP route, update `docs/reference/openapi.yaml` in the same change, or
//! this test goes red.
//!
//! Forward direction only (every route is documented). The reverse —
//! every documented path is a real route — can't be checked per-daemon
//! because the single spec also carries the sibling product's paths.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Walk up from this crate's dir until we find the repo root (the dir
/// holding `docs/reference/openapi.yaml`).
fn repo_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if dir.join("docs/reference/openapi.yaml").is_file() {
            return dir;
        }
        assert!(
            dir.pop(),
            "could not locate docs/reference/openapi.yaml above {}",
            env!("CARGO_MANIFEST_DIR")
        );
    }
}

/// Extract every path literal passed to `.route("...", ...)` in `src`,
/// normalizing axum `:param` segments to OpenAPI `{param}` form.
fn routes_in(src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    // `.route(` never matches `.route_layer(` (the next char is `_`),
    // so this picks up only real route registrations.
    for (idx, _) in src.match_indices(".route(") {
        let rest = &src[idx..];
        let open = rest.find('"').expect("route path opening quote");
        let after = &rest[open + 1..];
        let close = after.find('"').expect("route path closing quote");
        let raw = &after[..close];
        let norm = raw
            .split('/')
            .map(|seg| {
                if let Some(name) = seg.strip_prefix(':') {
                    format!("{{{name}}}")
                } else {
                    seg.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        out.insert(norm);
    }
    out
}

/// Top-level keys under `paths:` in the spec.
fn spec_paths(root: &Path) -> BTreeSet<String> {
    let text = std::fs::read_to_string(root.join("docs/reference/openapi.yaml"))
        .expect("read openapi.yaml");
    let doc: serde_yaml::Value = serde_yaml::from_str(&text).expect("openapi.yaml is valid YAML");
    doc.get("paths")
        .and_then(|p| p.as_mapping())
        .expect("openapi.yaml has a paths mapping")
        .keys()
        .map(|k| k.as_str().expect("path key is a string").to_string())
        .collect()
}

#[test]
fn every_tcp_route_is_documented() {
    let root = repo_root();
    let src = std::fs::read_to_string(root.join("vtl/daemon/src/http/mod.rs"))
        .expect("read vtl/daemon/src/http/mod.rs");
    let routes = routes_in(&src);
    assert!(
        !routes.is_empty(),
        "extracted zero routes — the parser or source layout changed"
    );
    let documented = spec_paths(&root);

    let missing: Vec<&String> = routes.iter().filter(|r| !documented.contains(*r)).collect();
    assert!(
        missing.is_empty(),
        "thurvtld TCP routes missing from docs/reference/openapi.yaml: {missing:?}\n\
         add them to the spec (and a sync-guard entry) in the same change"
    );
}
