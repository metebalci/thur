// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Sync guard for `docs/reference/openapi-admin.yaml` — the admin-socket mutating
//! contract subset the Kubernetes CSI driver (`csi/`) consumes (issue #15).
//!
//! Unlike `openapi_sync.rs` (which guards the read-only TCP surface in
//! `src/http.rs`), this guard covers the peer-cred Unix admin-socket router
//! (`src/admin/mod.rs`) — but only the explicit allowlist of routes the CSI
//! driver depends on, not the whole admin surface (NVMe-TCP, dhchap, target,
//! admin-password, …). It asserts:
//!   1. every contract route is a real route in `admin/mod.rs` (the allowlist
//!      can't name a route that has been renamed or removed), and
//!   2. `docs/reference/openapi-admin.yaml` documents exactly that contract subset — no
//!      undocumented contract route, no documented path without a backing
//!      route.
//! Change a route the driver uses -> update both the allowlist below and the
//! spec in the same change, or this test goes red.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The admin-socket routes the CSI driver depends on (path level; methods are
/// documented per-path in the spec). Mirror of `csi/pkg/vsa` call sites and the
/// `paths:` in `docs/reference/openapi-admin.yaml`.
const CSI_CONTRACT: &[&str] = &[
    "/api/v1/iscsi/users",
    "/api/v1/iscsi/users/grant",
    "/api/v1/iscsi/users/remove",
    "/api/v1/iscsi/users/revoke",
    "/api/v1/volumes",
    "/api/v1/volumes/{name}",
    "/api/v1/volumes/{name}/clone",
    "/api/v1/volumes/{name}/resize",
    "/api/v1/volumes/{name}/snapshots",
    "/api/v1/volumes/{name}/snapshots/{snap}",
];

/// Walk up from this crate's dir until we find the repo root (the dir holding
/// `docs/reference/openapi-admin.yaml`).
fn repo_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if dir.join("docs/reference/openapi-admin.yaml").is_file() {
            return dir;
        }
        assert!(
            dir.pop(),
            "could not locate docs/reference/openapi-admin.yaml above {}",
            env!("CARGO_MANIFEST_DIR")
        );
    }
}

/// Extract every path literal passed to `.route("...", ...)` in `src`,
/// normalizing axum `:param` segments to OpenAPI `{param}` form.
fn routes_in(src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
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

/// Top-level keys under `paths:` in the admin spec.
fn spec_paths(root: &Path) -> BTreeSet<String> {
    let text = std::fs::read_to_string(root.join("docs/reference/openapi-admin.yaml"))
        .expect("read openapi-admin.yaml");
    let doc: serde_yaml::Value =
        serde_yaml::from_str(&text).expect("openapi-admin.yaml is valid YAML");
    doc.get("paths")
        .and_then(|p| p.as_mapping())
        .expect("openapi-admin.yaml has a paths mapping")
        .keys()
        .map(|k| k.as_str().expect("path key is a string").to_string())
        .collect()
}

#[test]
fn csi_contract_routes_exist_and_match_spec() {
    let root = repo_root();
    let src = std::fs::read_to_string(root.join("vsa/daemon/src/admin/mod.rs"))
        .expect("read vsa/daemon/src/admin/mod.rs");
    let routes = routes_in(&src);
    let contract: BTreeSet<String> = CSI_CONTRACT.iter().map(|s| s.to_string()).collect();

    // (1) every contract route is a real admin route.
    let missing: Vec<&String> = contract.iter().filter(|r| !routes.contains(*r)).collect();
    assert!(
        missing.is_empty(),
        "CSI contract names routes not mounted in admin/mod.rs: {missing:?}\n\
         a driver-relied route was renamed/removed — fix the allowlist + spec"
    );

    // (2) the spec documents exactly the contract subset.
    let documented = spec_paths(&root);
    assert_eq!(
        documented,
        contract,
        "docs/reference/openapi-admin.yaml paths must equal the CSI contract subset\n\
         documented-but-not-contract: {:?}\n\
         contract-but-not-documented: {:?}",
        documented.difference(&contract).collect::<Vec<_>>(),
        contract.difference(&documented).collect::<Vec<_>>(),
    );
}
