// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Emits `THURVSA_VERSION=<crate-ver> (<sha>[-dirty])` so clap's
//! `#[command(version = ...)]` in `main.rs` and the startup banner
//! include the build's git SHA. Mirrors `vtl/daemon/build.rs`;
//! duplicated deliberately because a shared build-helper crate
//! isn't worth the workspace plumbing for ~30 lines.
//!
//! Outside a git checkout (e.g. distro tarball rebuild), SHA falls
//! back to `unknown` and dirty to false. Operators rebuilding from a
//! signed source archive get the SHA-baked-in story via the source
//! archive's signature, not via this string.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    // CARGO_PKG_VERSION is always populated by cargo at build time;
    // env!() resolves it at compile time of build.rs itself, so there
    // is no Result to unwrap.
    let pkg_ver = env!("CARGO_PKG_VERSION");
    let version = if dirty {
        format!("{pkg_ver} ({sha}-dirty)")
    } else {
        format!("{pkg_ver} ({sha})")
    };
    println!("cargo:rustc-env=THURVSA_VERSION={version}");

    // `.git/logs/HEAD`, not `.git/HEAD` — the latter only mutates on
    // branch swap, so commits on the active branch don't trigger a
    // rerun. `.git/logs/HEAD` is appended on every HEAD movement.
    // Layout C puts this crate at `<workspace>/vsa/daemon/`, so
    // the workspace root is two parents up.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let Some(workspace_root) = manifest.parent().and_then(|p| p.parent()) else {
        return;
    };
    let head_log = workspace_root.join(".git/logs/HEAD");
    if head_log.exists() {
        println!("cargo:rerun-if-changed={}", head_log.display());
    }
}
